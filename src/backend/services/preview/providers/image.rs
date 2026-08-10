//! Image preview: a header-only dimension probe, no pixel decode.
//!
//! The pixels are gpui's job — `img()` renders from the path. What this
//! provider owes the UI is the dimension label and, critically, the refusal:
//! a header promising more pixels than [`IMAGE_MAX_PIXELS`] never reaches the
//! renderer at all.

use std::io::BufReader;

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::{
    IMAGE_MAX_PIXELS, LoadCtx, PreviewKind, PreviewProvider, read,
};

pub struct Image;

impl PreviewProvider for Image {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Image
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;

        // gpui renders SVG natively and the `image` crate cannot size it —
        // hand the path over without a header probe.
        if ctx.ext == "svg" {
            return Ok(PreviewPayload::Image {
                path: ctx.path.to_path_buf(),
                dimensions: None,
            });
        }

        let (file, total) = read::open_read(ctx.path)?;
        // Header-only dimension probe — pixel data stays undecoded here.
        let dimensions = ::image::ImageReader::new(BufReader::new(file))
            .with_guessed_format()
            .ok()
            .and_then(|reader| reader.into_dimensions().ok());
        if let Some((w, h)) = dimensions {
            // Decompression-bomb guard: refuse to hand the renderer an image
            // whose header promises an absurd pixel count.
            if u64::from(w) * u64::from(h) > IMAGE_MAX_PIXELS {
                return Ok(PreviewPayload::TooLarge { size: total });
            }
        }
        Ok(PreviewPayload::Image {
            path: ctx.path.to_path_buf(),
            dimensions,
        })
    }
}
