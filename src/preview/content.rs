//! Plain-data preview payloads. Everything here is `Send + 'static` so it
//! can be produced on the background executor and handed to the UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{RenderImage, SharedString};

pub enum PreviewContent {
    /// Rendered by gpui's native `img()` from the (sanitized) path.
    Image {
        path: PathBuf,
        /// From the image header only — pixel data is never decoded here.
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
    #[allow(dead_code)]
    Diff(crate::services::git::types::DiffPayload),
    TooLarge {
        size: u64,
    },
    Error(SharedString),
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
