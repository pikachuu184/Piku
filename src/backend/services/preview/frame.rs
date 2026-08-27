//! Frames derived from a file *after* its preview has already loaded.
//!
//! The initial preview is one payload for one file, cached under
//! `(path, mtime, size)`. Two things the viewer needs do not fit that shape:
//!
//! * **Page N of a PDF.** The provider rasterizes page 1 only. Rendering the
//!   first twelve up front — which is what it used to do — burned eleven
//!   rasterizations the viewer never showed, spent ~60 MB of the preview
//!   cache's 96 MB budget on one document, and still could not reach page 13.
//! * **A rotated raster image.** gpui's `with_transformation` exists on `svg`
//!   only, not on `img`, so a rotated photo is different pixels rather than a
//!   different transform. They have to be produced somewhere, and that
//!   somewhere is a worker.
//!
//! Both are the same request: given an authorized path and a small
//! specification, return one decoded frame. So they are one call
//! ([`PreviewService::derive_frame`]) with one [`FrameSpec`], and the UI holds
//! one slot for the answer.
//!
//! Nothing here relaxes a provider's limits. A PDF page goes through the same
//! size cap, page cap, `catch_unwind` and V8-disabled binding as the initial
//! load; a rotation goes through the same bounded decode as the first one.

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::backend::services::preview::content::RawImage;
use crate::backend::services::preview::providers::{image as image_provider, pdf};

/// Which derived frame is wanted.
///
/// Deliberately tiny and plain: it crosses the UI/backend boundary, so it may
/// not name a renderer type (`ci/invariants.sh` gate 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameSpec {
    /// One page of a PDF, zero-indexed, turned `quarter_turns` × 90° clockwise.
    PdfPage { index: usize, quarter_turns: u8 },
    /// A raster image turned `quarter_turns` × 90° clockwise.
    ///
    /// `quarter_turns` of 0 is never requested — that is the cached payload —
    /// but it is representable and produces the unrotated frame rather than an
    /// error, because a viewer that resets rotation should not have to special
    /// case its way back.
    RotatedImage { quarter_turns: u8 },
}

impl FrameSpec {
    /// The turn count, normalized to 0..4. Everything downstream can then
    /// assume a legal value instead of re-checking.
    pub fn quarter_turns(self) -> u8 {
        match self {
            Self::PdfPage { quarter_turns, .. } | Self::RotatedImage { quarter_turns } => {
                quarter_turns % 4
            }
        }
    }
}

/// Produce the frame described by `spec`.
///
/// Runs on a blocking worker. `path` has already been authorized by the
/// service, exactly as a provider's has.
pub fn render(
    path: &std::path::Path,
    spec: FrameSpec,
    cancel: &Cancel,
) -> Result<RawImage, PreviewError> {
    cancel.check()?;
    match spec {
        FrameSpec::PdfPage { index, .. } => {
            pdf::render_single_page(path, index, spec.quarter_turns(), cancel)
        }
        FrameSpec::RotatedImage { .. } => {
            image_provider::render_rotated(path, spec.quarter_turns(), cancel)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_are_normalized_rather_than_rejected() {
        // The UI counts turns up and takes them mod 4; a value that arrived
        // un-normalized must still mean something sensible.
        assert_eq!(
            FrameSpec::RotatedImage { quarter_turns: 4 }.quarter_turns(),
            0
        );
        assert_eq!(
            FrameSpec::RotatedImage { quarter_turns: 7 }.quarter_turns(),
            3
        );
        assert_eq!(
            FrameSpec::PdfPage {
                index: 0,
                quarter_turns: 5
            }
            .quarter_turns(),
            1
        );
    }
}
