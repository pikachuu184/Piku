//! Synchronous, blocking preview loading. Always runs on the background
//! executor — the UI thread only ever receives the finished
//! [`PreviewContent`].
//!
//! Hardening policy for every parser in this file: input is untrusted, reads
//! are capped by the consts in [`crate::preview`], archives are listed but
//! never extracted or decompressed, malformed input degrades to
//! [`PreviewContent::Error`] or the hex fallback, and nothing panics.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::BufReader;
use std::path::Path;

use gpui::SharedString;

use crate::preview::content::{ArchiveItem, HexRow, PreviewContent};
use crate::preview::language::language_for_ext;
use crate::preview::sniff;
use crate::preview::{
    ARCHIVE_ENTRY_CAP, AUDIO_WAVEFORM_MAX_BYTES, CODE_HEAD_CAP, HEX_CAP, IMAGE_MAX_PIXELS,
    MARKDOWN_CAP, PreviewKind, STRUCTURED_CAP, WAVEFORM_BUCKETS,
};
use crate::storage::provider::StorageProvider as _;

/// Produce the preview payload for one file. Never fails — every error is a
/// renderable [`PreviewContent::Error`] (or a downgrade to hex).
pub fn load_preview(kind: PreviewKind, path: &Path, ext: &str) -> PreviewContent {
    match kind {
        PreviewKind::Image => load_image(path, ext),
        PreviewKind::Code => load_code(path, ext),
        PreviewKind::Markdown => load_markdown(path),
        PreviewKind::Structured => load_structured(path, ext),
        PreviewKind::Archive => load_archive(path),
        PreviewKind::AudioMeta => load_audio_meta(path),
        PreviewKind::VideoMeta => load_video_meta(path),
        PreviewKind::Pdf => crate::preview::pdf::load_pdf(path),
        PreviewKind::Hex => load_hex(path),
    }
}

fn read_head(path: &Path, cap: usize) -> Result<(Vec<u8>, u64), PreviewContent> {
    crate::storage::local()
        .read_head(path, cap)
        .map_err(|error| PreviewContent::Error(format!("{error:#}").into()))
}

fn load_image(path: &Path, ext: &str) -> PreviewContent {
    // gpui renders SVG natively and the `image` crate cannot size it —
    // hand the path over without a header probe.
    if ext == "svg" {
        return PreviewContent::Image { path: path.to_path_buf(), dimensions: None };
    }
    let (file, total) = match crate::storage::local().open_read(path) {
        Ok(pair) => pair,
        Err(error) => return PreviewContent::Error(format!("{error:#}").into()),
    };
    // Header-only dimension probe — pixel data stays undecoded here.
    let dimensions = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok());
    if let Some((w, h)) = dimensions {
        // Decompression-bomb guard: refuse to hand gpui an image whose
        // header promises an absurd pixel count.
        if u64::from(w) * u64::from(h) > IMAGE_MAX_PIXELS {
            return PreviewContent::TooLarge { size: total };
        }
    }
    PreviewContent::Image { path: path.to_path_buf(), dimensions }
}

fn load_code(path: &Path, ext: &str) -> PreviewContent {
    let (bytes, total) = match read_head(path, CODE_HEAD_CAP) {
        Ok(pair) => pair,
        Err(error) => return error,
    };
    if sniff::looks_binary(&bytes) {
        return hex_from(&bytes, total);
    }
    PreviewContent::Code {
        text: String::from_utf8_lossy(&bytes).into_owned().into(),
        language: language_for_ext(ext),
        truncated: (bytes.len() as u64) < total,
        total_size: total,
    }
}

fn load_markdown(path: &Path) -> PreviewContent {
    let (bytes, total) = match read_head(path, MARKDOWN_CAP) {
        Ok(pair) => pair,
        Err(error) => return error,
    };
    if sniff::looks_binary(&bytes) {
        return hex_from(&bytes, total);
    }
    PreviewContent::Markdown {
        source: String::from_utf8_lossy(&bytes).into_owned().into(),
        truncated: (bytes.len() as u64) < total,
    }
}

fn load_structured(path: &Path, ext: &str) -> PreviewContent {
    let (bytes, total) = match read_head(path, STRUCTURED_CAP) {
        Ok(pair) => pair,
        Err(error) => return error,
    };
    if sniff::looks_binary(&bytes) {
        return hex_from(&bytes, total);
    }
    let truncated = (bytes.len() as u64) < total;
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // JSON pretty-print, computed here so the UI thread never parses. Only
    // for complete documents — a truncated head is not valid JSON.
    let pretty = (ext == "json" && !truncated)
        .then(|| {
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|value| serde_json::to_string_pretty(&value).ok())
        })
        .flatten()
        .map(SharedString::from);
    PreviewContent::Structured {
        text: text.into(),
        language: language_for_ext(ext),
        pretty,
        truncated,
    }
}

fn load_archive(path: &Path) -> PreviewContent {
    let (file, _total) = match crate::storage::local().open_read(path) {
        Ok(pair) => pair,
        Err(error) => return PreviewContent::Error(format!("{error:#}").into()),
    };
    // Listing only: central-directory metadata is read, entry data is never
    // decompressed or extracted.
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(error) => return PreviewContent::Error(format!("not a readable zip: {error}").into()),
    };
    let total_count = archive.len();
    let listed = total_count.min(ARCHIVE_ENTRY_CAP);
    let mut entries = Vec::with_capacity(listed);
    for index in 0..listed {
        let Ok(entry) = archive.by_index_raw(index) else {
            continue;
        };
        entries.push(ArchiveItem {
            name: entry.name().to_string().into(),
            size: entry.size(),
            is_dir: entry.is_dir(),
        });
    }
    PreviewContent::Archive { entries, total_count, truncated: total_count > listed }
}

fn load_audio_meta(path: &Path) -> PreviewContent {
    use lofty::prelude::*;

    let (file, total) = match crate::storage::local().open_read(path) {
        Ok(pair) => pair,
        Err(error) => return PreviewContent::Error(format!("{error:#}").into()),
    };
    let probe = match lofty::probe::Probe::new(BufReader::new(file)).guess_file_type() {
        Ok(probe) => probe,
        Err(error) => {
            return PreviewContent::Error(format!("could not read audio metadata: {error}").into());
        }
    };
    let tagged = match probe.read() {
        Ok(tagged) => tagged,
        Err(error) => {
            return PreviewContent::Error(format!("could not read audio metadata: {error}").into());
        }
    };

    let mut rows: Vec<(SharedString, SharedString)> = Vec::new();
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let mut push = |label: &str, value: Option<std::borrow::Cow<'_, str>>| {
            if let Some(value) = value
                && !value.is_empty()
            {
                rows.push((label.to_string().into(), value.into_owned().into()));
            }
        };
        push("Title", tag.title());
        push("Artist", tag.artist());
        push("Album", tag.album());
        push("Genre", tag.genre());
    }
    let properties = tagged.properties();
    let duration = properties.duration();
    let seconds = duration.as_secs();
    rows.push(("Duration".into(), format!("{}:{:02}", seconds / 60, seconds % 60).into()));
    if let Some(bitrate) = properties.overall_bitrate() {
        rows.push(("Bitrate".into(), format!("{bitrate} kbps").into()));
    }
    if let Some(rate) = properties.sample_rate() {
        rows.push(("Sample rate".into(), format!("{rate} Hz").into()));
    }
    if let Some(channels) = properties.channels() {
        rows.push(("Channels".into(), channels.to_string().into()));
    }
    // Untrusted input: only re-decode the whole file for peaks when it is
    // within the size cap; otherwise the scrubber renders without a waveform.
    let waveform = if total <= AUDIO_WAVEFORM_MAX_BYTES {
        let mut peaks = audio_waveform(path);
        // rodio/symphonia can't decode every format lofty can still tag (some
        // WMA/AAC). When it yields nothing, fall back to the bundled ffmpeg,
        // which decodes far more — so the waveform still displays.
        if peaks.iter().all(|p| *p <= 0.0) {
            if let Some(fallback) =
                crate::services::video_probe::audio_pcm_peaks(path, WAVEFORM_BUCKETS)
            {
                peaks = fallback;
            }
        }
        peaks
    } else {
        Vec::new()
    };
    PreviewContent::Audio {
        rows,
        waveform,
        duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
    }
}

/// Coarse peak waveform (0..1, normalized) for the transport display. Decodes
/// the whole file on the background executor, building a max-amplitude envelope
/// that self-coarsens as it grows — so it needs **no** up-front length estimate
/// and a missing/zero duration can't collapse every sample into one bucket
/// (the previous bug). Capped so a pathological input can't spin forever.
/// Failure → empty (the caller then tries the ffmpeg fallback). rodio/symphonia
/// can *panic* on a malformed stream, so the decode runs inside `catch_unwind`
/// and a panic degrades to an empty waveform (then the ffmpeg fallback).
fn audio_waveform(path: &Path) -> Vec<f32> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| audio_waveform_decode(path)))
        .unwrap_or_default()
}

fn audio_waveform_decode(path: &Path) -> Vec<f32> {
    use rodio::Source as _;

    const MAX_SAMPLES: u64 = 60_000_000;

    let Ok((file, _)) = crate::storage::local().open_read(path) else {
        return Vec::new();
    };
    let Ok(decoder) = rodio::Decoder::new(BufReader::new(file)) else {
        return Vec::new();
    };
    let channels = u64::from(decoder.channels().max(1));
    let sample_rate = u64::from(decoder.sample_rate().max(1));
    // Start at ~100 envelope points/sec; the envelope halves (and the stride
    // doubles) whenever it gets too long, keeping memory bounded for any length.
    let mut stride = ((sample_rate * channels) / 100).max(1);

    let mut env: Vec<f32> = Vec::with_capacity(WAVEFORM_BUCKETS * 8);
    let mut cur = 0f32;
    let mut in_window: u64 = 0;
    let mut idx: u64 = 0;
    for sample in decoder {
        let amp = f32::from(sample).abs() / 32768.0;
        if amp > cur {
            cur = amp;
        }
        in_window += 1;
        if in_window >= stride {
            env.push(cur);
            cur = 0.0;
            in_window = 0;
            if env.len() >= WAVEFORM_BUCKETS * 8 {
                env = halve_envelope(&env);
                stride *= 2;
            }
        }
        idx += 1;
        if idx >= MAX_SAMPLES {
            break;
        }
    }
    if in_window > 0 {
        env.push(cur);
    }
    resample_peaks(&env, WAVEFORM_BUCKETS)
}

/// Merge adjacent envelope points by their max, halving the length. Used to
/// keep the growing envelope bounded without losing peak information.
fn halve_envelope(env: &[f32]) -> Vec<f32> {
    env.chunks(2)
        .map(|pair| pair.iter().copied().fold(0.0f32, f32::max))
        .collect()
}

/// Resample an arbitrary-length max-envelope to exactly `buckets` bars (taking
/// the max over each source range so peaks survive), then normalize to 0..1.
/// Handles both `env.len() < buckets` (stretch) and `> buckets` (downsample).
fn resample_peaks(env: &[f32], buckets: usize) -> Vec<f32> {
    let n = env.len();
    if n == 0 {
        return Vec::new();
    }
    let mut peaks = vec![0f32; buckets];
    for (b, peak) in peaks.iter_mut().enumerate() {
        let lo = (b * n) / buckets;
        let hi = (((b + 1) * n) / buckets).max(lo + 1).min(n);
        *peak = env[lo..hi].iter().copied().fold(0.0f32, f32::max);
    }
    let max = peaks.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for peak in peaks.iter_mut() {
            *peak /= max;
        }
    }
    peaks
}

fn load_video_meta(path: &Path) -> PreviewContent {
    // Real metadata for the MP4 family (mp4/mov/m4v) via the pure-Rust `mp4`
    // crate — duration, resolution, codecs, frame rate, bitrate. Other
    // containers (mkv/webm/avi/…) fall back to the sniffed container name;
    // size/dates already appear in the panel's detail rows.
    let rows = match crate::services::video_probe::mp4_metadata(path) {
        Some(rows) if !rows.is_empty() => rows,
        _ => {
            let (bytes, _total) = match read_head(path, 64) {
                Ok(pair) => pair,
                Err(error) => return error,
            };
            let format = sniff::sniff(&bytes).unwrap_or("Unknown container");
            vec![("Container".into(), format.to_string().into())]
        }
    };
    // Best-effort poster frame (needs the bundled ffmpeg); absent → no poster.
    let poster = crate::services::video_probe::poster_frame(path, None);
    PreviewContent::Video { rows, poster }
}

fn load_hex(path: &Path) -> PreviewContent {
    let (bytes, total) = match read_head(path, HEX_CAP) {
        Ok(pair) => pair,
        Err(error) => return error,
    };
    // Extensionless (or mislabeled) files whose magic bytes are a renderable
    // image get upgraded to a real image preview.
    if sniff::sniffed_renderable_image(&bytes) {
        return PreviewContent::Image { path: path.to_path_buf(), dimensions: None };
    }
    hex_from(&bytes, total)
}

/// Precompute display rows (offset · 16 hex bytes · ASCII gutter) so the UI
/// thread renders strings only.
fn hex_from(bytes: &[u8], total: u64) -> PreviewContent {
    let rows = bytes
        .chunks(16)
        .take(HEX_CAP / 16)
        .enumerate()
        .map(|(index, chunk)| {
            let mut hex = String::with_capacity(chunk.len() * 3);
            let mut ascii = String::with_capacity(chunk.len());
            for byte in chunk {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x} ");
                ascii.push(if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '·'
                });
            }
            HexRow {
                offset: format!("{:08x}", index * 16).into(),
                hex: hex.trim_end().to_string().into(),
                ascii: ascii.into(),
            }
        })
        .collect();
    PreviewContent::Hex { rows, signature: sniff::sniff(bytes), total_size: total }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: write real files to the temp dir and run the full
    /// sanitize → read → decode pipeline on them.
    #[test]
    fn loads_real_files() {
        let dir = std::env::temp_dir().join("piku-preview-tests");
        let _ = std::fs::create_dir_all(&dir);

        let rust = dir.join("sample.rs");
        let _ = std::fs::write(&rust, "fn main() { println!(\"hi\"); }\n");
        match load_preview(PreviewKind::Code, &rust, "rs") {
            PreviewContent::Code { language, truncated, .. } => {
                assert_eq!(language, Some("rust"));
                assert!(!truncated);
            }
            _ => unreachable!("expected Code content"),
        }

        let json = dir.join("sample.json");
        let _ = std::fs::write(&json, r#"{"b":1,"a":[1,2]}"#);
        match load_preview(PreviewKind::Structured, &json, "json") {
            PreviewContent::Structured { pretty, .. } => {
                assert!(pretty.is_some_and(|p| p.contains("\n")));
            }
            _ => unreachable!("expected Structured content"),
        }

        // A malformed "zip" must degrade to an error, never a panic.
        let bad_zip = dir.join("broken.zip");
        let _ = std::fs::write(&bad_zip, b"PK\x03\x04 this is not really a zip");
        assert!(matches!(
            load_preview(PreviewKind::Archive, &bad_zip, "zip"),
            PreviewContent::Error(_)
        ));

        // A real (empty) zip lists zero entries.
        let ok_zip = dir.join("ok.zip");
        let _ = std::fs::write(&ok_zip, b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0");
        match load_preview(PreviewKind::Archive, &ok_zip, "zip") {
            PreviewContent::Archive { total_count, .. } => assert_eq!(total_count, 0),
            _ => unreachable!("expected Archive content"),
        }

        // Binary bytes with a text extension downgrade to hex.
        let fake_txt = dir.join("binary.txt");
        let _ = std::fs::write(&fake_txt, b"MZ\x90\x00\x03\x00\x00\x00");
        assert!(matches!(
            load_preview(PreviewKind::Code, &fake_txt, "txt"),
            PreviewContent::Hex { signature: Some("Windows executable (PE)"), .. }
        ));

        // Missing file is an error, not a panic.
        assert!(matches!(
            load_preview(PreviewKind::Code, &dir.join("missing.rs"), "rs"),
            PreviewContent::Error(_)
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resample_spreads_peaks_across_all_buckets() {
        // A short envelope (fewer points than buckets) must stretch to fill
        // every bar — the zero-duration bug collapsed everything into one.
        let env = vec![0.2, 0.8, 0.4, 1.0];
        let peaks = resample_peaks(&env, 240);
        assert_eq!(peaks.len(), 240);
        // Normalized: the loudest source point becomes 1.0 somewhere.
        assert!((peaks.iter().copied().fold(0.0f32, f32::max) - 1.0).abs() < 1e-6);
        // Not all energy piled into the final bucket.
        assert!(peaks[..120].iter().any(|p| *p > 0.0));
    }

    #[test]
    fn resample_downsamples_keeping_peaks() {
        // A long envelope with one spike must keep that spike after downsample.
        let mut env = vec![0.1f32; 2000];
        env[1000] = 1.0;
        let peaks = resample_peaks(&env, 240);
        assert_eq!(peaks.len(), 240);
        assert!((peaks.iter().copied().fold(0.0f32, f32::max) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn halve_envelope_takes_pairwise_max() {
        assert_eq!(halve_envelope(&[0.1, 0.9, 0.5, 0.2, 0.7]), vec![0.9, 0.5, 0.7]);
    }

    #[test]
    fn resample_empty_is_empty() {
        assert!(resample_peaks(&[], 240).is_empty());
    }

    #[test]
    fn hex_rows_format() {
        let PreviewContent::Hex { rows, signature, total_size } =
            hex_from(b"MZ\x90\x00ABCDEFGHIJKL", 14)
        else {
            unreachable!("hex_from always returns Hex");
        };
        assert_eq!(total_size, 14);
        assert_eq!(signature, Some("Windows executable (PE)"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].offset.as_ref(), "00000000");
        assert!(rows[0].hex.as_ref().starts_with("4d 5a 90 00"));
        assert!(rows[0].ascii.as_ref().starts_with("MZ·"));
    }
}
