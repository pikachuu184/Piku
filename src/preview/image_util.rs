//! Conversions from decoded pixels to a gpui `RenderImage`.
//!
//! gpui stores image pixels as **BGRA**, so red and blue are swapped before the
//! buffer is wrapped — this mirrors what gpui's own image loader does
//! (`crates/gpui/src/elements/img.rs`).
//!
//! Two entry points, because the swap happens in two different places:
//!
//! * [`render_image_from_bgra`] takes a backend `RawImage`, whose bytes are
//!   **already swapped** on a worker thread. Preferred — the swap is a loop
//!   over every byte in the image and has no business on the UI thread.
//! * [`render_image_from_rgba`] takes a freshly decoded `RgbaImage` and swaps
//!   here. For the video player, which decodes its own frames and never goes
//!   through the preview engine.

use std::sync::Arc;

use gpui::RenderImage;

use crate::backend::services::preview::content::RawImage;

/// Wrap an already-BGRA backend buffer. No pixel work: `from_raw` and
/// `RenderImage::new` are both moves.
pub fn render_image_from_bgra(raw: RawImage) -> Arc<RenderImage> {
    // `from_raw` returns None only if the buffer length disagrees with the
    // dimensions, which `RawImage::from_rgba` makes impossible. Falling back to
    // a 1x1 rather than unwrapping keeps the crate's no-panic rule intact.
    let buffer = image::RgbaImage::from_raw(raw.width, raw.height, raw.bgra)
        .unwrap_or_else(|| image::RgbaImage::new(1, 1));
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// Wrap a decoded `RgbaImage`, swapping channels in place first.
pub fn render_image_from_rgba(mut rgba: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]))
}
