//! Plain-data preview payloads.
//!
//! Nothing here may name a gpui type (`ci/invariants.sh` gate 1). The UI-side
//! adapter in `crate::preview::content` converts these into the renderable
//! `PreviewContent` exactly once, at the point the answer crosses the thread
//! boundary.
//!
//! The split is by *cost*, not by taste:
//!
//! * **Text crosses as `Arc<str>`.** `SharedString` is a `SmolStr`, and
//!   `SmolStr::from(Arc<str>)` keeps the allocation and bumps a refcount for
//!   anything longer than the inline capacity. A 512 KiB preview therefore
//!   converts for free.
//! * **Pixels cross as [`RawImage`].** Wrapping one as a `RenderImage` is a
//!   move, so it is also free — but only because the *cache* stores the
//!   converted value. Caching the bytes instead would mean a `Vec<u8>` clone on
//!   every hit, which for a rendered PDF is hundreds of megabytes.

use std::path::PathBuf;
use std::sync::Arc;

/// One label/value row in the inspector's metadata list.
pub type MetaRow = (Arc<str>, Arc<str>);

/// A decoded image, ready to be wrapped as a gpui `RenderImage`.
///
/// **The bytes are already BGRA**, not RGBA: gpui stores pixels blue-first, and
/// the channel swap is a loop over every byte in the image, so it belongs on a
/// worker thread rather than on the UI thread. A field named `bgra` that
/// actually held RGBA would be a trap, hence the name and this paragraph.
pub struct RawImage {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

impl RawImage {
    /// Consume a decoded RGBA image, swapping channels in place.
    ///
    /// Mirrors what gpui's own image loader does
    /// (`crates/gpui/src/elements/img.rs`).
    pub fn from_rgba(mut rgba: image::RgbaImage) -> Self {
        let (width, height) = (rgba.width(), rgba.height());
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        Self {
            width,
            height,
            bgra: rgba.into_raw(),
        }
    }

    /// Bytes held resident. For the cache's byte budget, which today guesses
    /// with a flat per-image estimate.
    #[allow(dead_code, reason = "consumed when the cache budget stops guessing")]
    pub fn byte_len(&self) -> usize {
        self.bgra.len()
    }
}

impl std::fmt::Debug for RawImage {
    /// Hand-written so a payload dump does not print megabytes of pixels.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawImage")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.bgra.len())
            .finish()
    }
}

/// What a provider produces. One variant per [`super::PreviewKind`], plus
/// `TooLarge`.
///
/// Failure is **not** represented here. A provider returns
/// `Err(PreviewError)`, so a cancellation cannot be mistaken for content and
/// cached. `TooLarge` is not a failure — it is a successful classification
/// with its own rendering.
pub enum PreviewPayload {
    Image(ImagePreview),
    Code {
        text: Arc<str>,
        language: Option<&'static str>,
        truncated: bool,
        total_size: u64,
    },
    Markdown {
        source: Arc<str>,
        truncated: bool,
    },
    Structured {
        text: Arc<str>,
        language: Option<&'static str>,
        /// JSON only: the pretty-printed form, computed off-thread.
        pretty: Option<Arc<str>>,
        truncated: bool,
    },
    Archive {
        entries: Vec<ArchiveItem>,
        total_count: usize,
        truncated: bool,
    },
    /// Playable audio: tag/property rows, a normalized peak waveform (0..1),
    /// and the total duration for the transport bar.
    Audio {
        rows: Vec<MetaRow>,
        waveform: Vec<f32>,
        duration_ms: u64,
    },
    /// Video: metadata rows plus an optional decoded poster frame (present when
    /// the bundled ffmpeg produced one).
    Video {
        rows: Vec<MetaRow>,
        poster: Option<RawImage>,
    },
    /// Rendered PDF pages (up to a cap), the true page count, and a note when
    /// rendering was truncated or pdfium was unavailable.
    Pdf {
        pages: Vec<RawImage>,
        total_pages: usize,
        note: Option<Arc<str>>,
    },
    Hex {
        rows: Vec<HexRow>,
        signature: Option<&'static str>,
        total_size: u64,
    },
    TooLarge {
        size: u64,
    },
}

/// How an image preview reaches the screen.
///
/// Two arms, because SVG genuinely is different: the renderer draws it
/// natively at any size, and the `image` crate cannot rasterize it at all.
/// Everything else goes through the same hardened decode the thumbnails use —
/// which is the whole point. Applying EXIF orientation to thumbnails only
/// would leave a portrait photo upright in the grid and sideways in the
/// inspector, and being *inconsistently* wrong is worse than being uniformly
/// wrong.
pub enum ImagePreview {
    /// Hand the path to the renderer. SVG only.
    Path { path: PathBuf },
    /// Decoded here, orientation already applied.
    Decoded {
        image: RawImage,
        /// The **source** dimensions, for the size label — the decoded buffer
        /// may have been downscaled to fit the viewport.
        dimensions: (u32, u32),
    },
}

pub struct ArchiveItem {
    pub name: Arc<str>,
    pub size: u64,
    pub is_dir: bool,
}

pub struct HexRow {
    pub offset: Arc<str>,
    pub hex: Arc<str>,
    pub ascii: Arc<str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_image_swaps_red_and_blue_and_keeps_alpha() {
        // One opaque pure-red pixel: RGBA 255,0,0,255 -> BGRA 0,0,255,255.
        let rgba = image::RgbaImage::from_raw(1, 1, vec![255, 0, 0, 255]).expect("1x1 buffer");
        let raw = RawImage::from_rgba(rgba);
        assert_eq!(raw.bgra, vec![0, 0, 255, 255]);
        assert_eq!((raw.width, raw.height), (1, 1));
        assert_eq!(raw.byte_len(), 4);
    }

    #[test]
    fn a_raw_image_debug_does_not_print_its_pixels() {
        // A payload dump must stay readable; this is why Debug is hand-written.
        let rgba = image::RgbaImage::from_raw(2, 2, vec![9; 16]).expect("2x2 buffer");
        let text = format!("{:?}", RawImage::from_rgba(rgba));
        assert!(text.contains("bytes: 16"), "{text}");
        assert!(!text.contains("[9,"), "pixels leaked into Debug: {text}");
    }
}
