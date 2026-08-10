//! The one hardened image decode, shared by thumbnails, video posters and PDF
//! pages.
//!
//! # The defect this replaces
//!
//! The thumbnail path used to probe the header for a pixel count, refuse
//! anything over 100 megapixels, and then call plain `.decode()`. A
//! header-legal 99 MP image therefore passed the guard and allocated roughly
//! 400 MB of RGBA before anything else could object. The guard bounded the
//! wrong quantity: pixels, not bytes.
//!
//! # Two gates, and which one is load-bearing
//!
//! 1. **The header probe is strict.** It is ours, it runs before any decoder
//!    state exists, and it is what actually stops a decompression bomb.
//! 2. **`Limits` is belt, not braces.** `max_image_width`/`max_image_height`
//!    are documented strict — a decoder that cannot honour them must fail —
//!    but `max_alloc` is explicitly *non-strict*: "the library does not
//!    guarantee that limit will not be exceeded". So it is defence in depth
//!    behind the probe, never instead of it.
//!
//! `ImageDecoder::total_bytes()` sits between the two: it is the decoder's own
//! estimate of the output buffer, computed from dimensions and colour type
//! before a byte is allocated, so it catches the case where a format's pixels
//! are wider than the RGBA we assumed.
//!
//! # Orientation
//!
//! Phone photos carry their rotation in EXIF rather than in the pixel data.
//! Not applying it is why portrait photos rendered sideways. `image` 0.25.8
//! added `orientation()`/`apply_orientation()`; this is the only place that
//! calls them, so grid and inspector cannot disagree about which way is up.

use std::io::{BufReader, Seek as _};

use image::{DynamicImage, ImageDecoder as _, ImageReader, Limits};

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::backend::services::preview::content::RawImage;
use crate::backend::services::preview::read;

/// Bytes a single decode may ask for.
pub const MAX_ALLOC: u64 = 192 * 1024 * 1024;

/// Pixels a single decode may produce, **derived** from [`MAX_ALLOC`] so the
/// two cannot drift apart. RGBA8 is four bytes per pixel, so this is 48 MP —
/// comfortably past any consumer camera, and a factor of two below the 100 MP
/// cap it replaces, which permitted a ~400 MB allocation.
pub const MAX_PIXELS: u64 = MAX_ALLOC / 4;

/// Strict per-axis cap. Catches the degenerate 1 × 4_000_000_000 shape, which
/// satisfies a pixel-count check on its own but makes every row allocation
/// pathological.
pub const MAX_EDGE: u32 = 24_576;

/// A decoded image plus what the header claimed, before any downscale.
///
/// `Debug` prints the dimensions only — a `DynamicImage` would dump every
/// pixel, and these are routinely tens of megabytes.
pub struct Decoded {
    pub image: DynamicImage,
    /// Dimensions **after** orientation, i.e. what the user will see.
    pub dimensions: (u32, u32),
}

impl std::fmt::Debug for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Decoded")
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

/// Decode `path` under strict limits, applying EXIF orientation.
///
/// `max_bytes` is a per-caller file-size gate applied before anything is read;
/// pass `u64::MAX` to skip it.
pub fn decode_bounded(
    path: &std::path::Path,
    max_bytes: u64,
    cancel: &Cancel,
) -> Result<Decoded, PreviewError> {
    cancel.check()?;
    let (mut file, total) = read::open_read(path)?;
    if total == 0 || total > max_bytes {
        return Err(PreviewError::Undecodable(
            "image is too large to decode".into(),
        ));
    }

    // Gate 1, strict and ours: refuse the bomb before a decoder exists.
    let (w, h) = ImageReader::new(BufReader::new(&file))
        .with_guessed_format()
        .map_err(|e| undecodable("reading the image header", &e))?
        .into_dimensions()
        .map_err(|e| undecodable("reading the image dimensions", &e))?;
    if u64::from(w) * u64::from(h) > MAX_PIXELS || w > MAX_EDGE || h > MAX_EDGE {
        return Err(PreviewError::Undecodable(
            format!("image is {w}×{h}, past the decode limit")
                .as_str()
                .into(),
        ));
    }

    cancel.check()?;
    file.rewind()
        .map_err(|e| undecodable("rewinding the image", &e))?;
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|e| undecodable("reading the image header", &e))?;

    // `Limits` is #[non_exhaustive]: no struct literal and no
    // `..Default::default()` from this crate. Field-by-field on a Default is
    // the only legal construction, so it looks odd on purpose.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);

    // `into_decoder` applies the limits via `set_limits`, which is what makes
    // the strict dimension caps take effect inside the decoder itself.
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| undecodable("preparing the image decoder", &e))?;

    // Gate 2: the decoder's own output-buffer estimate, still before any
    // allocation. Catches formats whose pixels are wider than the RGBA the
    // pixel cap above assumed.
    if decoder.total_bytes() > MAX_ALLOC {
        return Err(PreviewError::Undecodable(
            "image needs more memory than the decode budget allows".into(),
        ));
    }

    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image =
        DynamicImage::from_decoder(decoder).map_err(|e| undecodable("decoding the image", &e))?;
    // Before measuring: a 90° rotation swaps width and height, and the label
    // the inspector shows must match what is on screen.
    image.apply_orientation(orientation);

    let dimensions = (image.width(), image.height());
    Ok(Decoded { image, dimensions })
}

/// Decode and downscale to a `target`-edge thumbnail, ready for the UI.
pub fn thumbnail(
    path: &std::path::Path,
    target: u32,
    max_bytes: u64,
    cancel: &Cancel,
) -> Result<RawImage, PreviewError> {
    let decoded = decode_bounded(path, max_bytes, cancel)?;
    cancel.check()?;
    // `thumbnail` is a fast, aspect-preserving downscale bounded by
    // target × target.
    Ok(RawImage::from_rgba(
        decoded.image.thumbnail(target, target).into_rgba8(),
    ))
}

fn undecodable(what: &str, error: &dyn std::fmt::Display) -> PreviewError {
    PreviewError::Undecodable(format!("{what}: {error}").as_str().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder as _, Rgba, RgbaImage};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("piku-preview-decode-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// A *structurally valid* PNG that lies about its size: a real 1×1 image
    /// whose IHDR is rewritten to claim `w × h`, with the CRC recomputed so no
    /// integrity check rejects it first.
    ///
    /// The first attempt at this fixture emitted IHDR + IEND with no pixel
    /// data, and the decoder refused it with "IDAT chunk is missing" — which
    /// would have made the test pass for entirely the wrong reason. A
    /// decompression bomb is a *well-formed* file; the fixture has to be one
    /// too, or it proves nothing about the guard.
    fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        // PNG layout: 8-byte signature, then IHDR as length(4) + "IHDR"(4) +
        // data(13) + CRC(4). Width and height are the first eight data bytes.
        const IHDR_TYPE: usize = 12;
        const IHDR_DATA: usize = 16;
        const IHDR_CRC: usize = 29;

        let mut bytes = Vec::new();
        let one = RgbaImage::new(1, 1);
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(one.as_raw(), 1, 1, image::ExtendedColorType::Rgba8)
            .expect("encode the 1x1 base image");

        bytes[IHDR_DATA..IHDR_DATA + 4].copy_from_slice(&w.to_be_bytes());
        bytes[IHDR_DATA + 4..IHDR_DATA + 8].copy_from_slice(&h.to_be_bytes());
        let crc = crc32(&bytes[IHDR_TYPE..IHDR_CRC]);
        bytes[IHDR_CRC..IHDR_CRC + 4].copy_from_slice(&crc.to_be_bytes());
        bytes
    }

    /// CRC-32 as PNG defines it, over chunk type + data.
    fn crc32(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (n, entry) in table.iter_mut().enumerate() {
            let mut c = n as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        let mut c = 0xFFFF_FFFFu32;
        for byte in data {
            c = table[((c ^ u32::from(*byte)) & 0xFF) as usize] ^ (c >> 8);
        }
        c ^ 0xFFFF_FFFF
    }

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        let mut buf = RgbaImage::new(w, h);
        for (x, y, p) in buf.enumerate_pixels_mut() {
            *p = Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255]);
        }
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(buf.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .expect("encode");
        let _ = std::fs::write(path, bytes);
    }

    /// The budgets are derived from one another; this fails if someone edits
    /// one without the other.
    #[test]
    fn the_pixel_cap_and_the_alloc_cap_agree() {
        assert_eq!(
            MAX_PIXELS * 4,
            MAX_ALLOC,
            "MAX_PIXELS must stay MAX_ALLOC worth of RGBA8"
        );
        assert!(
            u64::from(MAX_EDGE) * u64::from(MAX_EDGE) > MAX_PIXELS,
            "the edge cap must not be the binding constraint for square images"
        );
    }

    /// The defect. A header claiming more pixels than the budget must be
    /// refused *before* a pixel buffer exists — the file below has no pixel
    /// data at all, so reaching the decode would fail differently.
    #[test]
    fn a_header_that_claims_more_pixels_than_the_budget_is_refused_before_decoding() {
        let dir = scratch("bomb");
        let bomb = dir.join("bomb.png");
        // 20000 × 20000 = 400 MP, four bytes each: 1.6 GB if decoded.
        let _ = std::fs::write(&bomb, png_claiming(20_000, 20_000));

        let error = decode_bounded(&bomb, u64::MAX, &Cancel::never())
            .expect_err("a 400 MP header must be refused");
        let text = error.to_string();
        assert!(
            text.contains("20000×20000"),
            "the refusal should name the claimed size, got: {text}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_wider_than_the_strict_edge_cap_is_refused() {
        let dir = scratch("wide");
        let wide = dir.join("wide.png");
        // Only ~1.6 MP, so the pixel cap alone would allow it; the edge cap is
        // what catches this shape.
        let _ = std::fs::write(&wide, png_claiming(MAX_EDGE + 1, 64));
        assert!(
            decode_bounded(&wide, u64::MAX, &Cancel::never()).is_err(),
            "a degenerate aspect ratio slipped past the edge cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ordinary_image_decodes_and_thumbnails() {
        let dir = scratch("ordinary");
        let file = dir.join("photo.png");
        write_png(&file, 200, 100);

        let decoded = decode_bounded(&file, u64::MAX, &Cancel::never()).expect("decode");
        assert_eq!(decoded.dimensions, (200, 100));

        let thumb = thumbnail(&file, 96, u64::MAX, &Cancel::never()).expect("thumbnail");
        assert!(thumb.width <= 96 && thumb.height <= 96, "{thumb:?}");
        assert_eq!(
            thumb.bgra.len(),
            (thumb.width * thumb.height * 4) as usize,
            "buffer length must match the dimensions"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_without_exif_is_left_unrotated() {
        let dir = scratch("no-exif");
        let file = dir.join("plain.png");
        write_png(&file, 8, 4);
        let decoded = decode_bounded(&file, u64::MAX, &Cancel::never()).expect("decode");
        assert_eq!(decoded.dimensions, (8, 4), "an unrotated image moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Orientation 6 is "rotate 90° clockwise", the one every phone produces
    /// for a portrait photo. Before this, those rendered sideways.
    #[test]
    fn a_jpeg_with_exif_orientation_6_is_rotated_upright() {
        let dir = scratch("exif");
        let file = dir.join("portrait.jpg");
        let Some(bytes) = jpeg_with_orientation(8, 4, 6) else {
            eprintln!("could not build the EXIF fixture — skipping");
            return;
        };
        let _ = std::fs::write(&file, bytes);

        let decoded = decode_bounded(&file, u64::MAX, &Cancel::never()).expect("decode");
        assert_eq!(
            decoded.dimensions,
            (4, 8),
            "orientation 6 must swap the axes; got {:?}",
            decoded.dimensions
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An 8×4 JPEG carrying an EXIF APP1 segment with the given orientation.
    fn jpeg_with_orientation(w: u32, h: u32, orientation: u16) -> Option<Vec<u8>> {
        let mut baseline = Vec::new();
        let mut buf = RgbaImage::new(w, h);
        for p in buf.pixels_mut() {
            *p = Rgba([10, 20, 30, 255]);
        }
        let rgb = DynamicImage::ImageRgba8(buf).to_rgb8();
        image::codecs::jpeg::JpegEncoder::new(&mut baseline)
            .write_image(rgb.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .ok()?;

        // TIFF header + one IFD entry (0x0112 Orientation, SHORT, count 1).
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II\x2a\x00");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());

        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut out = Vec::new();
        // SOI, then our APP1, then the rest of the baseline JPEG.
        out.extend_from_slice(&baseline[..2]);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&baseline[2..]);
        Some(out)
    }

    #[test]
    fn an_already_cancelled_decode_does_no_work() {
        let dir = scratch("cancelled");
        let file = dir.join("photo.png");
        write_png(&file, 64, 64);
        assert!(matches!(
            decode_bounded(&file, u64::MAX, &Cancel::already()),
            Err(PreviewError::Cancelled)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_over_the_size_gate_is_refused_without_reading_it() {
        let dir = scratch("too-big");
        let file = dir.join("photo.png");
        write_png(&file, 64, 64);
        assert!(decode_bounded(&file, 8, &Cancel::never()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_non_image_degrades_to_an_error_rather_than_a_panic() {
        let dir = scratch("garbage");
        let file = dir.join("not-an-image.png");
        let _ = std::fs::write(&file, b"this is definitely not a png");
        assert!(decode_bounded(&file, u64::MAX, &Cancel::never()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
