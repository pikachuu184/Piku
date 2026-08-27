//! Conversions from decoded pixels to a gpui `RenderImage`.
//!
//! gpui stores image pixels as **BGRA**, so red and blue are swapped before the
//! buffer is wrapped — this mirrors what gpui's own image loader does
//! (`crates/gpui/src/elements/img.rs`).
//!
//! Three entry points, because the swap happens in three different places:
//!
//! * [`render_image_from_bgra`] takes a backend `RawImage`, whose bytes are
//!   **already swapped** on a worker thread. Preferred — the swap is a loop
//!   over every byte in the image and has no business on the UI thread.
//! * [`render_image_from_bgra_bytes`] wraps a raw BGRA buffer that arrived
//!   already in gpui's order, with no `RawImage` in between. For the video
//!   player, whose ffmpeg emits `bgra` directly.
//! * [`render_image_from_rgba`] takes a freshly decoded `RgbaImage` and swaps
//!   here, for callers that cannot choose their pixel order.

use std::sync::Arc;

use gpui::RenderImage;

use crate::backend::services::preview::content::RawImage;

/// Wrap an already-BGRA backend buffer. No pixel work: `from_raw` and
/// `RenderImage::new` are both moves.
pub fn render_image_from_bgra(raw: RawImage) -> Arc<RenderImage> {
    render_image_from_bgra_bytes(raw.width, raw.height, raw.bgra)
}

/// Wrap a raw BGRA buffer of `width × height` pixels. No pixel work.
///
/// Returns `None` when the buffer length disagrees with the dimensions, which is
/// the honest answer for a caller reading frames off a pipe: a truncated frame
/// should be skipped, not silently shown as a 1×1 tile.
pub fn render_image_from_bgra_bytes_checked(
    width: u32,
    height: u32,
    bgra: Vec<u8>,
) -> Option<Arc<RenderImage>> {
    let buffer = image::RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
}

/// Wrap a raw BGRA buffer, falling back to a 1×1 tile on a length mismatch.
///
/// The fallback rather than an unwrap keeps the crate's no-panic rule intact;
/// `RawImage::from_rgba` makes the mismatch impossible for its own callers.
pub fn render_image_from_bgra_bytes(width: u32, height: u32, bgra: Vec<u8>) -> Arc<RenderImage> {
    render_image_from_bgra_bytes_checked(width, height, bgra).unwrap_or_else(|| {
        Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]))
    })
}

/// Wrap a decoded `RgbaImage`, swapping channels in place first.
#[allow(
    dead_code,
    reason = "kept for decoders that cannot choose their pixel order"
)]
pub fn render_image_from_rgba(mut rgba: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]))
}
