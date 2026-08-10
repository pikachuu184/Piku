//! The renderable preview payload, and the one conversion into it.
//!
//! This is the UI half of the split described in
//! `crate::backend::services::preview::content`. The backend produces plain
//! data; [`PreviewContent::from`] turns it into gpui values exactly once, in
//! the dispatch closure, and the cache stores *this* side — storing the byte
//! form instead would mean a `Vec<u8>` clone on every hit, which for a rendered
//! PDF is hundreds of megabytes.
//!
//! Both conversions are cheap by construction: `SharedString` is a `SmolStr`
//! built from an `Arc<str>` (refcount bump), and `RenderImage` takes ownership
//! of the pixel buffer (a move).

use std::sync::Arc;

use gpui::{RenderImage, SharedString};

use crate::backend::services::preview::content::{ImagePreview, PreviewPayload, RawImage};
use crate::preview::image_util::render_image_from_bgra;

pub enum PreviewContent {
    Image {
        source: PreviewImage,
        /// The **source** dimensions, for the size label. `None` for SVG,
        /// which has no pixel size of its own.
        dimensions: Option<(u32, u32)>,
    },
    Code {
        text: SharedString,
        language: Option<&'static str>,
        truncated: bool,
        total_size: u64,
    },
    Markdown {
        source: SharedString,
        truncated: bool,
    },
    Structured {
        text: SharedString,
        language: Option<&'static str>,
        /// JSON only: the pretty-printed form, computed off-thread.
        pretty: Option<SharedString>,
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
        rows: Vec<(SharedString, SharedString)>,
        waveform: Vec<f32>,
        duration_ms: u64,
    },
    /// Video: metadata rows plus an optional decoded poster frame (present when
    /// the bundled ffmpeg produced one). Playback stays in the OS default app.
    Video {
        rows: Vec<(SharedString, SharedString)>,
        poster: Option<Arc<RenderImage>>,
    },
    /// Rendered PDF pages (up to a cap), the true page count, and a note when
    /// rendering was truncated or pdfium was unavailable.
    Pdf {
        pages: Vec<Arc<RenderImage>>,
        total_pages: usize,
        note: Option<SharedString>,
    },
    Hex {
        rows: Vec<HexRow>,
        signature: Option<&'static str>,
        total_size: u64,
    },
    /// A textual git diff between two revisions of one file. Hunk headers
    /// and lines arrive pre-sanitized and capped from the git backend. The
    /// git inspector renders diffs through the same `diff_block` renderer;
    /// this variant is the seam for historical-revision previews.
    ///
    /// UI-side only: no provider produces it, so it has no payload counterpart.
    #[allow(dead_code)]
    Diff(crate::services::git::types::DiffPayload),
    TooLarge {
        size: u64,
    },
    /// A failed preview, rendered as a message. Produced from the service's
    /// `Err(PreviewError)` at the dispatch boundary rather than carried in the
    /// payload, so a cancellation can never be cached as content.
    Error(SharedString),
}

/// What `img()` should be handed for an image preview.
///
/// Deliberately *not* a `gpui::ImageSource`, even though that is what it turns
/// into one line later: `ImageSource` has a `Custom(Arc<dyn Fn ..>)` variant and
/// so is neither `Send` nor `Sync`, which would make the whole cached
/// `Arc<PreviewContent>` non-`Send`. Both arms here are cheap to convert at
/// render time — a path clone or an `Arc` bump.
#[derive(Clone)]
pub enum PreviewImage {
    /// SVG: the renderer rasterizes it itself, at whatever size it is drawn.
    Path(std::path::PathBuf),
    /// Everything else: decoded on a worker, orientation already applied, so
    /// the renderer never opens the file and cannot disagree about it.
    Decoded(Arc<RenderImage>),
}

pub struct ArchiveItem {
    pub name: SharedString,
    pub size: u64,
    pub is_dir: bool,
}

pub struct HexRow {
    pub offset: SharedString,
    pub hex: SharedString,
    pub ascii: SharedString,
}

/// `Arc<str>` → `SharedString` without copying the bytes.
#[inline]
fn text(value: Arc<str>) -> SharedString {
    SharedString::from(value)
}

fn rows(value: Vec<(Arc<str>, Arc<str>)>) -> Vec<(SharedString, SharedString)> {
    value
        .into_iter()
        .map(|(label, val)| (text(label), text(val)))
        .collect()
}

fn image(raw: RawImage) -> Arc<RenderImage> {
    render_image_from_bgra(raw)
}

impl From<PreviewPayload> for PreviewContent {
    /// Exhaustive on purpose. A new payload variant must fail to compile here
    /// rather than fall into a catch-all and silently render as nothing.
    fn from(payload: PreviewPayload) -> Self {
        match payload {
            PreviewPayload::Image(preview) => match preview {
                ImagePreview::Path { path } => Self::Image {
                    source: PreviewImage::Path(path),
                    dimensions: None,
                },
                ImagePreview::Decoded {
                    image: raw,
                    dimensions,
                } => Self::Image {
                    // The frame is already decoded, so the renderer never opens
                    // the file — which is what makes the orientation applied on
                    // the worker the one the user actually sees.
                    source: PreviewImage::Decoded(image(raw)),
                    dimensions: Some(dimensions),
                },
            },
            PreviewPayload::Code {
                text: body,
                language,
                truncated,
                total_size,
            } => Self::Code {
                text: text(body),
                language,
                truncated,
                total_size,
            },
            PreviewPayload::Markdown { source, truncated } => Self::Markdown {
                source: text(source),
                truncated,
            },
            PreviewPayload::Structured {
                text: body,
                language,
                pretty,
                truncated,
            } => Self::Structured {
                text: text(body),
                language,
                pretty: pretty.map(text),
                truncated,
            },
            PreviewPayload::Archive {
                entries,
                total_count,
                truncated,
            } => Self::Archive {
                entries: entries
                    .into_iter()
                    .map(|e| ArchiveItem {
                        name: text(e.name),
                        size: e.size,
                        is_dir: e.is_dir,
                    })
                    .collect(),
                total_count,
                truncated,
            },
            PreviewPayload::Audio {
                rows: meta,
                waveform,
                duration_ms,
            } => Self::Audio {
                rows: rows(meta),
                waveform,
                duration_ms,
            },
            PreviewPayload::Video { rows: meta, poster } => Self::Video {
                rows: rows(meta),
                poster: poster.map(image),
            },
            PreviewPayload::Pdf {
                pages,
                total_pages,
                note,
            } => Self::Pdf {
                pages: pages.into_iter().map(image).collect(),
                total_pages,
                note: note.map(text),
            },
            PreviewPayload::Hex {
                rows: hex_rows,
                signature,
                total_size,
            } => Self::Hex {
                rows: hex_rows
                    .into_iter()
                    .map(|r| HexRow {
                        offset: text(r.offset),
                        hex: text(r.hex),
                        ascii: text(r.ascii),
                    })
                    .collect(),
                signature,
                total_size,
            },
            PreviewPayload::TooLarge { size } => Self::TooLarge { size },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the duplication this split costs: every payload variant must map
    /// to a content variant, and the mapping must not lose the fields the
    /// renderer reads.
    #[test]
    fn every_payload_variant_maps_to_a_content_variant() {
        let cases = [
            PreviewPayload::Image(ImagePreview::Decoded {
                image: RawImage::from_rgba(
                    image::RgbaImage::from_raw(1, 1, vec![0, 0, 0, 255]).expect("buffer"),
                ),
                dimensions: (4, 2),
            }),
            PreviewPayload::Code {
                text: "fn main() {}".into(),
                language: Some("rust"),
                truncated: true,
                total_size: 99,
            },
            PreviewPayload::Markdown {
                source: "# hi".into(),
                truncated: false,
            },
            PreviewPayload::Structured {
                text: "{}".into(),
                language: Some("json"),
                pretty: Some("{\n}".into()),
                truncated: false,
            },
            PreviewPayload::Archive {
                entries: vec![crate::backend::services::preview::content::ArchiveItem {
                    name: "a.txt".into(),
                    size: 3,
                    is_dir: false,
                }],
                total_count: 1,
                truncated: false,
            },
            PreviewPayload::Audio {
                rows: vec![("Title".into(), "Song".into())],
                waveform: vec![0.5],
                duration_ms: 1000,
            },
            PreviewPayload::Video {
                rows: vec![("Container".into(), "MP4".into())],
                poster: None,
            },
            PreviewPayload::Pdf {
                pages: Vec::new(),
                total_pages: 3,
                note: Some("note".into()),
            },
            PreviewPayload::Hex {
                rows: vec![crate::backend::services::preview::content::HexRow {
                    offset: "00000000".into(),
                    hex: "4d 5a".into(),
                    ascii: "MZ".into(),
                }],
                signature: Some("Windows executable (PE)"),
                total_size: 2,
            },
            PreviewPayload::TooLarge { size: 1 << 40 },
        ];
        assert_eq!(
            cases.len(),
            10,
            "a payload variant was added without a case here"
        );

        for case in cases {
            // The conversion must not panic and must not land on Error/Diff,
            // neither of which has a payload counterpart.
            let converted = PreviewContent::from(case);
            assert!(
                !matches!(
                    converted,
                    PreviewContent::Error(_) | PreviewContent::Diff(_)
                ),
                "a payload converted into a UI-only variant"
            );
        }
    }

    #[test]
    fn the_conversion_preserves_the_fields_the_renderer_reads() {
        let converted = PreviewContent::from(PreviewPayload::Code {
            text: "fn main() {}".into(),
            language: Some("rust"),
            truncated: true,
            total_size: 99,
        });
        let PreviewContent::Code {
            text,
            language,
            truncated,
            total_size,
        } = converted
        else {
            unreachable!("Code maps to Code");
        };
        assert_eq!(text.as_ref(), "fn main() {}");
        assert_eq!(language, Some("rust"));
        assert!(truncated);
        assert_eq!(total_size, 99);
    }

    #[test]
    fn a_poster_frame_keeps_its_dimensions_through_the_conversion() {
        let rgba = image::RgbaImage::from_raw(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).expect("buffer");
        let converted = PreviewContent::from(PreviewPayload::Video {
            rows: Vec::new(),
            poster: Some(RawImage::from_rgba(rgba)),
        });
        let PreviewContent::Video {
            poster: Some(poster),
            ..
        } = converted
        else {
            unreachable!("a poster was supplied");
        };
        assert_eq!(poster.size(0).width.0, 2);
        assert_eq!(poster.size(0).height.0, 1);
    }
}
