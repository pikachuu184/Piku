//! Pure-Rust video metadata extraction (no ffmpeg required).
//!
//! The `mp4` crate reads the MP4/MOV/M4V container header — duration,
//! resolution, codecs, frame rate, bitrate — without decoding any frames.
//! Poster-frame extraction (which does need a codec) is a future enhancement
//! gated on ffmpeg being available; other containers fall back to the sniffed
//! container name in the loader.

use std::io::{BufReader, Read as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use gpui::{RenderImage, SharedString};

use crate::preview::image_util::render_image_from_rgba;

/// Whether a runnable ffmpeg binary is resolvable (bundled next to the exe, in
/// the sidecar cache, or on PATH). Probed once — the check itself spawns a
/// process, so it must not run per-thumbnail.
fn ffmpeg_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(ffmpeg_sidecar::command::ffmpeg_is_installed)
}

/// Cap on captured poster PNG bytes — a single frame is at most a few MiB even
/// at 4K; this bounds a hostile stream.
const POSTER_MAX_BYTES: u64 = 64 * 1024 * 1024;
const POSTER_TIMEOUT: Duration = Duration::from_secs(15);

/// Extract a poster frame ~1s into the video with the bundled ffmpeg, decode
/// it, and optionally downscale to a `target`-edge thumbnail. Returns `None`
/// when ffmpeg is unavailable or extraction fails. Blocking — background
/// executor only. Uses no temp files (frame is piped through stdout) and kills
/// the subprocess if it exceeds [`POSTER_TIMEOUT`].
pub fn poster_frame(path: &Path, target: Option<u32>) -> Option<Arc<RenderImage>> {
    if !ffmpeg_available() {
        return None;
    }
    let mut cmd = Command::new(ffmpeg_sidecar::paths::ffmpeg_path());
    cmd.args(["-nostdin", "-loglevel", "error", "-ss", "1", "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
    }

    let png = run_capture(cmd)?;
    if png.is_empty() {
        return None;
    }
    let decoded = image::load_from_memory(&png).ok()?;
    let rgba = match target {
        Some(t) => decoded.thumbnail(t, t).into_rgba8(),
        None => decoded.into_rgba8(),
    };
    Some(render_image_from_rgba(rgba))
}

/// Spawn `cmd`, reading its stdout on a helper thread (so a full pipe never
/// deadlocks the wait) while enforcing [`POSTER_TIMEOUT`]; the child is killed
/// on timeout. Returns the captured bytes only on a clean exit.
fn run_capture(mut cmd: Command) -> Option<Vec<u8>> {
    let mut child = cmd.spawn().ok()?;
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.take(POSTER_MAX_BYTES).read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() > POSTER_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(_) => break None,
        }
    };
    let out = reader.join().ok()?;
    status?.success().then_some(out)
}

/// Metadata rows for an MP4-family file, or `None` if it is not MP4/MOV or the
/// header cannot be parsed. The third-party parser runs inside `catch_unwind`
/// so a malformed/hostile file degrades gracefully instead of aborting.
pub fn mp4_metadata(path: &Path) -> Option<Vec<(SharedString, SharedString)>> {
    let (file, total) = crate::storage::local().open_read(path).ok()?;
    if total == 0 {
        return None;
    }

    let parsed = catch_unwind(AssertUnwindSafe(|| {
        mp4::Mp4Reader::read_header(BufReader::new(file), total)
    }));
    let mp4 = parsed.ok()?.ok()?;

    let mut rows: Vec<(SharedString, SharedString)> = Vec::new();
    let secs = mp4.duration().as_secs();
    rows.push(("Duration".into(), format!("{}:{:02}", secs / 60, secs % 60).into()));

    for track in mp4.tracks().values() {
        match track.track_type() {
            Ok(mp4::TrackType::Video) => {
                rows.push((
                    "Resolution".into(),
                    format!("{} × {}", track.width(), track.height()).into(),
                ));
                if let Ok(media) = track.media_type() {
                    rows.push(("Video codec".into(), media.to_string().into()));
                }
                let fps = track.frame_rate();
                if fps.is_finite() && fps > 0.0 {
                    rows.push(("Frame rate".into(), format!("{fps:.2} fps").into()));
                }
                let bitrate = track.bitrate();
                if bitrate > 0 {
                    rows.push(("Bitrate".into(), format!("{} kbps", bitrate / 1000).into()));
                }
            }
            Ok(mp4::TrackType::Audio) => {
                if let Ok(media) = track.media_type() {
                    rows.push(("Audio codec".into(), media.to_string().into()));
                }
            }
            _ => {}
        }
    }

    Some(rows)
}
