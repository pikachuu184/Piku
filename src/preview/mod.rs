//! The preview engine: decides how a selected file should be previewed and
//! loads bounded, decoded content for the inspector panel to render.
//!
//! Architecture: a closed set of providers behind one enum. `decide_kind`
//! picks a [`PreviewKind`] from the entry's extension, `loader::load_preview`
//! produces a plain-data [`content::PreviewContent`] on the background
//! executor (never the UI thread), and the panel matches on it to render.
//! Every provider treats the file as an untrusted parser input: reads are
//! size-capped, archives are listed but never extracted, malformed input
//! degrades to an error or the hex fallback, and nothing here panics.

pub mod content;
pub mod image_util;
pub mod language;
pub mod loader;
pub mod pdf;
pub mod sniff;

use crate::core::entry::{EntryKind, FsEntry};

/// Hard caps every provider honours. Previews show a file's head, not the
/// whole file — the caps keep a multi-GB selection O(small).
pub const CODE_HEAD_CAP: usize = 256 * 1024;
pub const MARKDOWN_CAP: usize = 512 * 1024;
pub const STRUCTURED_CAP: usize = 512 * 1024;
pub const HEX_CAP: usize = 4 * 1024;
pub const ARCHIVE_ENTRY_CAP: usize = 1_000;
/// Building the preview waveform re-decodes the whole audio file; skip it for
/// anything larger than this so a pathological input can't burn the background
/// executor (the metadata rows still render; the scrubber just has no peaks).
pub const AUDIO_WAVEFORM_MAX_BYTES: u64 = 300 * 1024 * 1024;
/// Number of bars in the preview waveform / scrubber envelope.
pub const WAVEFORM_BUCKETS: usize = 240;
/// Only the (future) rotation path decodes pixels on our side; refuse
/// anything bigger than this outright.
#[allow(dead_code)]
pub const IMAGE_DECODE_CAP: u64 = 20 * 1024 * 1024;
/// Decompression-bomb guard: never hand gpui an image whose header claims
/// more pixels than this (width × height).
pub const IMAGE_MAX_PIXELS: u64 = 100_000_000;
/// PDF hardening: refuse documents larger than this outright, and never render
/// more than [`PDF_MAX_PAGES`] pages (pdfium is a C parser on untrusted input).
pub const PDF_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub const PDF_MAX_PAGES: usize = 2_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
    Code,
    Markdown,
    Structured,
    Archive,
    AudioMeta,
    VideoMeta,
    Pdf,
    /// Universal fallback: hex + ASCII head with a sniffed signature.
    Hex,
}

/// Image formats gpui's native `img()` element renders from a path.
const RENDERABLE_IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "svg", "tiff", "tif",
];

/// Structured text formats that read best with highlighting + (for JSON) a
/// pretty toggle rather than as generic source code.
const STRUCTURED_EXTS: &[&str] = &[
    "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf", "lock",
];

/// Extension-first provider choice. The loader may still downgrade text
/// kinds to [`PreviewKind::Hex`] when the bytes turn out to be binary, or
/// upgrade an extensionless file whose magic bytes are a renderable image.
pub fn decide_kind(entry: &FsEntry) -> PreviewKind {
    debug_assert!(entry.kind == EntryKind::File);
    kind_for_ext(entry.ext.as_str())
}

/// Provider choice for a bare path (used where no [`FsEntry`] is available, e.g.
/// the media panel opened directly on a file). The extension is lower-cased to
/// match the [`FsEntry`] convention.
pub fn kind_for_path(path: &std::path::Path) -> PreviewKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    kind_for_ext(&ext)
}

/// The extension-driven core shared by [`decide_kind`] and [`kind_for_path`].
pub fn kind_for_ext(ext: &str) -> PreviewKind {
    if RENDERABLE_IMAGE_EXTS.contains(&ext) {
        return PreviewKind::Image;
    }
    if matches!(ext, "md" | "markdown") {
        return PreviewKind::Markdown;
    }
    if STRUCTURED_EXTS.contains(&ext) {
        return PreviewKind::Structured;
    }
    if ext == "zip" {
        return PreviewKind::Archive;
    }
    if ext == "pdf" {
        return PreviewKind::Pdf;
    }
    use crate::core::file_type::{FileCategory, categorize_ext};
    match categorize_ext(ext) {
        FileCategory::Audio => PreviewKind::AudioMeta,
        FileCategory::Video => PreviewKind::VideoMeta,
        FileCategory::Code => PreviewKind::Code,
        // Plain-text documents preview as text; binary docs (pdf/docx/…)
        // fall through to hex until a dedicated provider exists.
        FileCategory::Document if matches!(ext, "txt" | "log" | "csv" | "rtf") => {
            PreviewKind::Code
        }
        _ => PreviewKind::Hex,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(name: &str) -> FsEntry {
        let path = PathBuf::from(format!("C:\\t\\{name}"));
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        FsEntry {
            name: name.to_string(),
            path,
            kind: EntryKind::File,
            size: 10,
            modified: None,
            created: None,
            hidden: false,
            readonly: false,
            ext,
        }
    }

    #[test]
    fn kinds_follow_extension() {
        assert_eq!(decide_kind(&entry("a.png")), PreviewKind::Image);
        assert_eq!(decide_kind(&entry("a.rs")), PreviewKind::Code);
        assert_eq!(decide_kind(&entry("a.md")), PreviewKind::Markdown);
        assert_eq!(decide_kind(&entry("a.json")), PreviewKind::Structured);
        assert_eq!(decide_kind(&entry("a.zip")), PreviewKind::Archive);
        assert_eq!(decide_kind(&entry("a.mp3")), PreviewKind::AudioMeta);
        assert_eq!(decide_kind(&entry("a.mp4")), PreviewKind::VideoMeta);
        assert_eq!(decide_kind(&entry("a.exe")), PreviewKind::Hex);
        assert_eq!(decide_kind(&entry("a.pdf")), PreviewKind::Pdf);
        assert_eq!(decide_kind(&entry("a.txt")), PreviewKind::Code);
        assert_eq!(decide_kind(&entry("noext")), PreviewKind::Hex);
    }
}
