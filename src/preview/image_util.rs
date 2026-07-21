//! One audited conversion from a decoded RGBA image to a gpui `RenderImage`,
//! shared by the thumbnail cache, PDF page rendering, and video posters.
//!
//! gpui stores image pixels as **BGRA**, so the red and blue channels are
//! swapped in place before the buffer is wrapped — this mirrors what gpui's own
//! image loader does (`crates/gpui/src/elements/img.rs`).

use std::sync::Arc;

use gpui::RenderImage;

/// Wrap a decoded `RgbaImage` as a single-frame BGRA `RenderImage`.
pub fn render_image_from_rgba(mut rgba: image::RgbaImage) -> Arc<RenderImage> {
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]))
}
