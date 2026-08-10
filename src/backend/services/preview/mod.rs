//! The preview engine: decides how a file should be previewed and produces
//! bounded, decoded, plain-data content for the inspector to render.
//!
//! Nothing under this module may name a gpui type (`ci/invariants.sh` gate 1).
//! The engine produces [`content::PreviewPayload`]; the UI-side adapter in
//! `crate::preview::content` turns that into the renderable `PreviewContent`
//! exactly once.
//!
//! Every provider treats the file as an untrusted parser input: reads are
//! size-capped, archives are listed but never extracted, malformed input
//! degrades to a typed error or the hex fallback, and nothing here panics —
//! the two third-party parsers that *can* panic (rodio/symphonia, pdfium) are
//! wrapped in `catch_unwind` at their call sites.

pub mod content;
pub mod language;
pub mod probe;
pub mod providers;
pub mod read;
pub mod service;
pub mod sniff;

use std::path::Path;

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::core::entry::{EntryKind, FsEntry};
use content::PreviewPayload;

pub use service::{PreviewKey, PreviewRequest, PreviewService};

/// Hard caps every provider honours. Previews show a file's head, not the
/// whole file — the caps keep a multi-GB selection O(small).
pub const CODE_HEAD_CAP: usize = 256 * 1024;
pub const MARKDOWN_CAP: usize = 512 * 1024;
pub const STRUCTURED_CAP: usize = 512 * 1024;
pub const HEX_CAP: usize = 4 * 1024;
pub const ARCHIVE_ENTRY_CAP: usize = 1_000;
/// Building the preview waveform re-decodes the whole audio file; skip it for
/// anything larger than this so a pathological input can't burn a worker (the
/// metadata rows still render; the scrubber just has no peaks).
pub const AUDIO_WAVEFORM_MAX_BYTES: u64 = 300 * 1024 * 1024;
/// Number of bars in the preview waveform / scrubber envelope.
pub const WAVEFORM_BUCKETS: usize = 240;
/// Decompression-bomb guard: never hand the renderer an image whose header
/// claims more pixels than this (width × height).
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

impl PreviewKind {
    /// Every variant, for table tests. Adding a variant without extending this
    /// fails `every_kind_has_a_provider_that_reports_its_own_kind`.
    ///
    /// `allow`, not `expect`: exercised by the tests below but not by the
    /// binary, so an expectation would be unfulfilled in the test target.
    #[allow(dead_code, reason = "table-test vocabulary")]
    pub const ALL: [PreviewKind; 9] = [
        PreviewKind::Image,
        PreviewKind::Code,
        PreviewKind::Markdown,
        PreviewKind::Structured,
        PreviewKind::Archive,
        PreviewKind::AudioMeta,
        PreviewKind::VideoMeta,
        PreviewKind::Pdf,
        PreviewKind::Hex,
    ];
}

/// What a provider is handed.
///
/// `path` is **already authorized** — the dispatcher re-validates it inside the
/// blocking closure before a provider ever runs, so a provider never sees a
/// path the policy would refuse and never needs the policy itself.
pub struct LoadCtx<'a> {
    pub path: &'a Path,
    pub ext: &'a str,
    pub cancel: &'a Cancel,
}

/// One parser, one kind.
///
/// The shared envelope — authorization, span, concurrency permit, cancellation
/// handle — belongs to the dispatcher, not here. That separation is what makes
/// `every_provider_stops_when_the_request_is_already_cancelled` expressible at
/// all; against nine free functions with nine signatures it is not.
pub trait PreviewProvider: Send + Sync {
    /// Which kind this provider serves. Exists so the routing table can be
    /// asserted total; production code reaches providers through
    /// [`provider_for`] and never asks. `allow`, not `expect`, for the same
    /// reason as [`PreviewKind::ALL`].
    #[allow(dead_code, reason = "asserted by the routing table test")]
    fn kind(&self) -> PreviewKind;
    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError>;
}

/// The single exhaustive routing point.
///
/// Deliberately a `match` over unit structs rather than a `HashMap`: a map
/// lookup returns `Option`, and under `#![deny(clippy::panic)]` the miss arm
/// could only degrade silently — turning "someone added a kind and forgot the
/// provider" from a compile error into a blank preview at runtime.
pub fn provider_for(kind: PreviewKind) -> &'static dyn PreviewProvider {
    match kind {
        PreviewKind::Image => &providers::image::Image,
        PreviewKind::Code => &providers::code::Code,
        PreviewKind::Markdown => &providers::markdown::Markdown,
        PreviewKind::Structured => &providers::structured::Structured,
        PreviewKind::Archive => &providers::archive::Archive,
        PreviewKind::AudioMeta => &providers::audio::AudioMeta,
        PreviewKind::VideoMeta => &providers::video::VideoMeta,
        PreviewKind::Pdf => &providers::pdf::Pdf,
        PreviewKind::Hex => &providers::hex::Hex,
    }
}

/// Produce the preview payload for one already-authorized file.
pub fn load(
    kind: PreviewKind,
    path: &Path,
    ext: &str,
    cancel: &Cancel,
) -> Result<PreviewPayload, PreviewError> {
    provider_for(kind).load(&LoadCtx { path, ext, cancel })
}

/// Image formats gpui's native image element renders from a path.
const RENDERABLE_IMAGE_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "webp", "ico", "svg", "tiff", "tif",
];

/// Structured text formats that read best with highlighting + (for JSON) a
/// pretty toggle rather than as generic source code.
const STRUCTURED_EXTS: &[&str] = &[
    "json", "yaml", "yml", "toml", "xml", "ini", "cfg", "conf", "lock",
];

/// Extension-first provider choice. The provider may still downgrade text
/// kinds to [`PreviewKind::Hex`] when the bytes turn out to be binary, or
/// upgrade an extensionless file whose magic bytes are a renderable image.
pub fn decide_kind(entry: &FsEntry) -> PreviewKind {
    debug_assert!(entry.kind == EntryKind::File);
    kind_for_ext(entry.ext.as_str())
}

/// Provider choice for a bare path (used where no [`FsEntry`] is available,
/// e.g. the media panel opened directly on a file). The extension is
/// lower-cased to match the [`FsEntry`] convention.
pub fn kind_for_path(path: &Path) -> PreviewKind {
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
        FileCategory::Document if matches!(ext, "txt" | "log" | "csv" | "rtf") => PreviewKind::Code,
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("piku-preview-mod-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
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

    /// The registry's reason for existing: routing must be total, and each
    /// provider must agree about which kind it serves.
    #[test]
    fn every_kind_has_a_provider_that_reports_its_own_kind() {
        for kind in PreviewKind::ALL {
            assert_eq!(
                provider_for(kind).kind(),
                kind,
                "{kind:?} is routed to a provider that claims to be something else"
            );
        }
    }

    /// The whole point of Stage 7. Every provider must observe cancellation
    /// before it does any work — this is impossible to state against the nine
    /// free functions this replaced.
    #[test]
    fn every_provider_stops_when_the_request_is_already_cancelled() {
        let dir = scratch("cancelled");
        // A real file so a provider that ignored the token would succeed and
        // make this test fail loudly rather than pass vacuously.
        let file = dir.join("sample.txt");
        let _ = std::fs::write(&file, b"hello world\n");

        for kind in PreviewKind::ALL {
            let result = load(kind, &file, "txt", &Cancel::already());
            assert!(
                matches!(result, Err(PreviewError::Cancelled)),
                "{kind:?} did not stop for an already-cancelled request"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Providers must refuse the things `read` refuses, whichever entry point
    /// they use — several open the file themselves rather than via `read_head`.
    #[test]
    fn every_provider_refuses_a_directory() {
        let dir = scratch("directory");
        for kind in PreviewKind::ALL {
            // SVG short-circuits before any read by design (gpui renders it
            // natively), so it is not part of this claim.
            let ext = if kind == PreviewKind::Image {
                "png"
            } else {
                "txt"
            };
            let result = load(kind, &dir, ext, &Cancel::never());
            assert!(
                result.is_err(),
                "{kind:?} accepted a directory as a preview input"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end through the real sanitize → read → decode pipeline.
    #[test]
    fn loads_real_files() {
        let dir = scratch("real");
        let never = Cancel::never();

        let rust = dir.join("sample.rs");
        let _ = std::fs::write(&rust, "fn main() { println!(\"hi\"); }\n");
        match load(PreviewKind::Code, &rust, "rs", &never) {
            Ok(PreviewPayload::Code {
                language,
                truncated,
                ..
            }) => {
                assert_eq!(language, Some("rust"));
                assert!(!truncated);
            }
            _ => unreachable!("expected Code content"),
        }

        let json = dir.join("sample.json");
        let _ = std::fs::write(&json, r#"{"b":1,"a":[1,2]}"#);
        match load(PreviewKind::Structured, &json, "json", &never) {
            Ok(PreviewPayload::Structured { pretty, .. }) => {
                assert!(pretty.is_some_and(|p| p.contains('\n')));
            }
            _ => unreachable!("expected Structured content"),
        }

        // A malformed "zip" must degrade to a typed error, never a panic.
        let bad_zip = dir.join("broken.zip");
        let _ = std::fs::write(&bad_zip, b"PK\x03\x04 this is not really a zip");
        assert!(matches!(
            load(PreviewKind::Archive, &bad_zip, "zip", &never),
            Err(PreviewError::Undecodable(_))
        ));

        // A real (empty) zip lists zero entries.
        let ok_zip = dir.join("ok.zip");
        let _ = std::fs::write(&ok_zip, b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        match load(PreviewKind::Archive, &ok_zip, "zip", &never) {
            Ok(PreviewPayload::Archive { total_count, .. }) => assert_eq!(total_count, 0),
            _ => unreachable!("expected Archive content"),
        }

        // Binary bytes with a text extension downgrade to hex.
        let fake_txt = dir.join("binary.txt");
        let _ = std::fs::write(&fake_txt, b"MZ\x90\x00\x03\x00\x00\x00");
        assert!(matches!(
            load(PreviewKind::Code, &fake_txt, "txt", &never),
            Ok(PreviewPayload::Hex {
                signature: Some("Windows executable (PE)"),
                ..
            })
        ));

        // Missing file is an error, not a panic.
        assert!(matches!(
            load(PreviewKind::Code, &dir.join("missing.rs"), "rs", &never),
            Err(PreviewError::File(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
