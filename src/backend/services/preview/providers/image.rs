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
use crate::backend::services::preview::content::{ImagePreview, PreviewPayload, RawImage};
use crate::backend::services::preview::decode::{self, DecodeError};
use crate::backend::services::preview::{LoadCtx, PreviewKind, PreviewProvider};

/// File-size gate for a full preview. Larger than the thumbnail gate (32 MiB)
/// because the inspector is where someone deliberately goes to look at one
/// image, and smaller than no gate at all.
pub const PREVIEW_MAX_BYTES: u64 = 96 * 1024 * 1024;

/// Longest edge of the decoded preview buffer.
///
/// Sized for the viewport, not for the file: the image box is 240 px tall and
/// zoom tops out at 8×, so 2048 is sharp at maximum magnification. Decoding a
/// 48 MP photo at full resolution would cost ~192 MB for a 240 px box and would
/// evict the entire preview cache (budget: 96 MB) on a single selection.
const PREVIEW_MAX_EDGE: u32 = 2048;

pub struct Image;

impl PreviewProvider for Image {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Image
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;

        if ctx.ext == "svg" {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::protocol::Cancel;
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
