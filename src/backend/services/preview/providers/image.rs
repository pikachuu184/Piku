//! Image preview.
//!
//! This goes through the **same** hardened decode as thumbnails, deliberately.
//! It used to hand the path straight to the renderer with only a header probe,
//! which meant two different decoders with two different limits and — once EXIF
//! orientation was applied to thumbnails — two different ideas of which way a
//! photo is up. A portrait photo upright in the grid and sideways in the
//! inspector is worse than one that is uniformly wrong.
//!
//! SVG is the one exception: the renderer draws it natively at any size and the
//! `image` crate cannot rasterize it, so it keeps the path route.

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::backend::services::preview::content::{ImagePreview, PreviewPayload, RawImage};
use crate::backend::services::preview::decode::{self, DecodeError};
use crate::backend::services::preview::{LoadCtx, PreviewKind, PreviewProvider, read};

/// File-size gate for a full preview. Larger than the thumbnail gate (32 MiB)
/// because the inspector is where someone deliberately goes to look at one
/// image, and smaller than no gate at all.
pub const PREVIEW_MAX_BYTES: u64 = 96 * 1024 * 1024;

/// Longest edge of the decoded preview buffer.
///
/// Sized for the cache, not for the file. One buffer at this edge costs
/// 2048 × 2048 × 4 ≈ 16 MiB of RGBA, and the preview cache's budget is 96 MiB
/// (`services::preview_cache::BYTE_BUDGET`) — so this size lets several images
/// stay resident while a 48 MP photo decoded at full resolution (~192 MB) would
/// evict everything on a single selection.
///
/// What this size does **not** promise is pixel-for-pixel sharpness at maximum
/// zoom. It used to: the inspector's image box was a fixed 240 px tall, and
/// 240 × 8 (the zoom cap) fits inside 2048 with room to spare. The box is now
/// the full height of the panel, so that arithmetic no longer holds and the
/// preview becomes resolution-limited once magnification asks for more source
/// pixels than were decoded. The UI says so rather than quietly showing a soft
/// image — see `zoom_is_resolution_limited` in the inspector. Serving real
/// pixels there needs a second, larger derivative fetched on demand, which
/// needs `PreviewRequest` to carry a desired size; it does not today.
pub const PREVIEW_MAX_EDGE: u32 = 2048;

/// File-size gate for an SVG.
///
/// SVG is the one preview whose bytes this crate never parses: the renderer
/// rasterizes it itself (see the module note), so none of [`decode`]'s pixel or
/// allocation budgets apply to it. That makes the file size the only bound we
/// can place on the work, and it matters more now that the preview fills the
/// panel rather than a 240 px box. Generous for hand-authored vector art,
/// far below anything that would keep the rasterizer busy.
pub const SVG_MAX_BYTES: u64 = 8 * 1024 * 1024;

pub struct Image;

impl PreviewProvider for Image {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Image
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;

        if ctx.ext == "svg" {
            // The renderer gets a path, so it — not this crate — opens the
            // file. That would skip `read::open_read`, and with it the two
            // refusals every other provider gets for free: a symlink is never
            // followed, and a fifo or device node is never opened. The service
            // validates paths *lexically* on purpose (canonicalizing would
            // resolve the link and hand the provider its target), so this is
            // the layer that has to catch it. Open it here for the refusals and
            // the size, then drop the handle and hand over the path as before.
            let (_file, total) = read::open_read(ctx.path)?;
            if total > SVG_MAX_BYTES {
                return Ok(PreviewPayload::TooLarge { size: total });
            }
            return Ok(PreviewPayload::Image(ImagePreview::Path {
                path: ctx.path.to_path_buf(),
            }));
        }

        match decode::decode_bounded(ctx.path, PREVIEW_MAX_BYTES, ctx.cancel) {
            Ok(decoded) => {
                let dimensions = decoded.dimensions;
                // Downscale only when it would actually help; `thumbnail` is a
                // no-op when the image already fits inside the bound.
                let buffer = decoded
                    .image
                    .thumbnail(PREVIEW_MAX_EDGE, PREVIEW_MAX_EDGE)
                    .into_rgba8();
                Ok(PreviewPayload::Image(ImagePreview::Decoded {
                    image: RawImage::from_rgba(buffer),
                    // The *source* size, so the label keeps reporting what the
                    // file is rather than what was decoded for the viewport.
                    dimensions,
                }))
            }
            // A successful classification with its own rendering, not an error.
            Err(DecodeError::TooLarge { size }) => Ok(PreviewPayload::TooLarge { size }),
            Err(DecodeError::Failed(error)) => Err(error),
        }
    }
}

/// Decode `path` and turn it `quarter_turns` × 90° clockwise.
///
/// Rotation is new pixels rather than a transform because gpui's
/// `with_transformation` exists on `svg` only, not on `img` — so there is no way
/// to turn a raster image in the renderer. Doing it here means it happens on a
/// worker, under the same decode budget as the first load, and reuses the same
/// `apply_orientation` path `decode.rs` already uses for EXIF.
///
/// The result is downscaled to [`PREVIEW_MAX_EDGE`] exactly as [`Image::load`]
/// does, so a rotated frame costs the cache no more than the unrotated one.
pub fn render_rotated(
    path: &std::path::Path,
    quarter_turns: u8,
    cancel: &Cancel,
) -> Result<RawImage, PreviewError> {
    let decoded = match decode::decode_bounded(path, PREVIEW_MAX_BYTES, cancel) {
        Ok(decoded) => decoded,
        // [`Image::load`] renders the size refusal as its own state; a derived
        // frame has no such state, and it is only ever asked for after that load
        // succeeded — so reaching here means the file grew underneath us.
        Err(DecodeError::TooLarge { .. }) => {
            return Err(PreviewError::Undecodable(
                "This image is now too large to render.".into(),
            ));
        }
        Err(DecodeError::Failed(error)) => return Err(error),
    };
    cancel.check()?;

    let mut image = decoded.image;
    if let Some(orientation) = quarter_turn_orientation(quarter_turns) {
        image.apply_orientation(orientation);
    }
    cancel.check()?;
    Ok(RawImage::from_rgba(
        image
            .thumbnail(PREVIEW_MAX_EDGE, PREVIEW_MAX_EDGE)
            .into_rgba8(),
    ))
}

/// Quarter turns as an `image` orientation, or `None` for no turn.
///
/// `Orientation` is the crate's own rotate/flip vocabulary and
/// `apply_orientation` is its in-place application — the same pair `decode.rs`
/// uses to honour an EXIF tag, so a user rotation and a camera rotation go
/// through one code path.
fn quarter_turn_orientation(quarter_turns: u8) -> Option<image::metadata::Orientation> {
    use image::metadata::Orientation;
    match quarter_turns % 4 {
        1 => Some(Orientation::Rotate90),
        2 => Some(Orientation::Rotate180),
        3 => Some(Orientation::Rotate270),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::error::FileError;
    use crate::backend::services::preview::{PreviewKind, load};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("piku-preview-image-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn write_png(path: &std::path::Path, w: u32, h: u32) {
        use image::ImageEncoder as _;
        let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(buf.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .expect("encode");
        let _ = std::fs::write(path, bytes);
    }

    /// The consistency guarantee: a raster image is decoded *here*, so the
    /// frame the inspector shows is the same one — same limits, same EXIF
    /// handling — that the grid thumbnail came from. If this ever went back to
    /// handing over a path, the renderer would decode it a second way.
    #[test]
    fn a_raster_image_is_decoded_rather_than_handed_over_as_a_path() {
        let dir = scratch("raster");
        let file = dir.join("photo.png");
        write_png(&file, 40, 20);

        let payload = load(PreviewKind::Image, &file, "png", &Cancel::never()).expect("preview");
        let PreviewPayload::Image(ImagePreview::Decoded { dimensions, .. }) = payload else {
            unreachable!("a raster image must be decoded, not passed through as a path");
        };
        assert_eq!(
            dimensions,
            (40, 20),
            "the label must report the source size"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// SVG is the one exception, and it has to stay one: the `image` crate
    /// cannot rasterize it at all.
    #[test]
    fn an_svg_keeps_the_path_route() {
        let dir = scratch("svg");
        let file = dir.join("logo.svg");
        let _ = std::fs::write(&file, b"<svg xmlns='http://www.w3.org/2000/svg'/>");

        let payload = load(PreviewKind::Image, &file, "svg", &Cancel::never()).expect("preview");
        assert!(matches!(
            payload,
            PreviewPayload::Image(ImagePreview::Path { .. })
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The SVG shortcut hands a *path* to the renderer, so this provider is the
    /// only thing standing between a symlink and the rasterizer opening its
    /// target. The service validates lexically on purpose, so if this check
    /// goes away nothing else catches it.
    #[cfg(unix)]
    #[test]
    fn an_svg_symlink_is_refused_rather_than_followed() {
        let dir = scratch("svg-symlink");
        let real = dir.join("real.svg");
        let _ = std::fs::write(&real, b"<svg xmlns='http://www.w3.org/2000/svg'/>");
        let link = dir.join("link.svg");
        let _ = std::os::unix::fs::symlink(&real, &link);

        match load(PreviewKind::Image, &link, "svg", &Cancel::never()) {
            Err(PreviewError::File(FileError::IsSymlink(_))) => {}
            Err(other) => unreachable!("expected IsSymlink, got {other:?}"),
            Ok(_) => unreachable!("a symlinked SVG must be refused, not previewed"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An SVG is never decoded here, so its file size is the only bound on the
    /// rasterization it will cause. Past the gate it is classified, not parsed.
    #[test]
    fn an_svg_past_the_size_gate_reports_too_large() {
        let dir = scratch("svg-too-large");
        let file = dir.join("huge.svg");
        let mut bytes = b"<svg xmlns='http://www.w3.org/2000/svg'>".to_vec();
        bytes.resize(SVG_MAX_BYTES as usize + 1, b' ');
        let _ = std::fs::write(&file, &bytes);

        let payload = load(PreviewKind::Image, &file, "svg", &Cancel::never()).expect("preview");
        assert!(
            matches!(payload, PreviewPayload::TooLarge { size } if size > SVG_MAX_BYTES),
            "an oversized SVG must be refused before the renderer sees the path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Past the budget is a classification with its own rendering, not a
    /// failure — the inspector shows the "too large" box rather than an error.
    #[test]
    fn an_image_past_the_budget_reports_too_large_rather_than_failing() {
        let dir = scratch("too-large");
        let file = dir.join("photo.png");
        write_png(&file, 64, 64);

        // Force the size gate by decoding with a deliberately tiny budget.
        match decode::decode_bounded(&file, 4, &Cancel::never()) {
            Err(DecodeError::TooLarge { size }) => assert!(size > 4),
            other => unreachable!("expected TooLarge, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
