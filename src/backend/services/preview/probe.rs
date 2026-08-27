//! Pure-Rust video metadata extraction (no ffmpeg required).
//!
//! The `mp4` crate reads the MP4/MOV/M4V container header — duration,
//! resolution, codecs, frame rate, bitrate — without decoding any frames.
//! Poster-frame extraction (which does need a codec) is a future enhancement
//! gated on ffmpeg being available; other containers fall back to the sniffed
//! container name in the loader.

use std::io::{BufReader, Read as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::backend::protocol::Cancel;

use super::content::{MetaRow, RawImage};

/// Whether a runnable ffmpeg binary is resolvable (bundled next to the exe, in
/// the sidecar cache, or on PATH). Probed once — the check itself spawns a
/// process, so it must not run per-thumbnail.
///
/// Public so the playback side shares this one cached probe rather than paying
/// for its own process spawn.
pub fn ffmpeg_available() -> bool {
    static AVAIL: OnceLock<bool> = OnceLock::new();
    *AVAIL.get_or_init(ffmpeg_sidecar::command::ffmpeg_is_installed)
}

/// Cap on captured poster PNG bytes — a single frame is at most a few MiB even
/// at 4K; this bounds a hostile stream.
const POSTER_MAX_BYTES: u64 = 64 * 1024 * 1024;
const POSTER_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on a sanitized filename stem used to build a saved-frame path.
const SAFE_STEM_CAP: usize = 64;

/// How many saved frames to keep in the screenshot cache. Nothing else ever
/// deletes from it, so without a bound it grows for the life of the install.
const SCREENSHOT_KEEP: usize = 100;

/// Extract a poster frame ~1s into the video with the bundled ffmpeg, decode
/// it, and optionally downscale to a `target`-edge thumbnail. Returns `None`
/// when ffmpeg is unavailable or extraction fails. Blocking — background
/// executor only. Uses no temp files (frame is piped through stdout) and kills
/// the subprocess if it exceeds [`POSTER_TIMEOUT`].
pub fn poster_frame(path: &Path, target: Option<u32>, cancel: &Cancel) -> Option<RawImage> {
    if !ffmpeg_available() {
        return None;
    }
    let mut cmd = Command::new(ffmpeg_sidecar::paths::ffmpeg_path());
    // `-protocol_whitelist file,crypto`: a crafted container must not be able to
    // make ffmpeg reach out to a remote URL (SSRF) — only local file access.
    cmd.args([
        "-nostdin",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,crypto",
        "-ss",
        "1",
        "-i",
    ])
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

    let png = run_capture(cmd, cancel)?;
    if png.is_empty() {
        return None;
    }
    let decoded = image::load_from_memory(&png).ok()?;
    let rgba = match target {
        Some(t) => decoded.thumbnail(t, t).into_rgba8(),
        None => decoded.into_rgba8(),
    };
    Some(RawImage::from_rgba(rgba))
}

/// Grab the frame at `at_ms` as a PNG and write it into the app-managed
/// screenshot cache dir (restrictive per-user location), returning the path.
/// Decoupled from live playback so a screenshot always works. Blocking —
/// background executor only; same subprocess hardening as [`poster_frame`].
pub fn save_frame_png(path: &Path, at_ms: u64, cancel: &Cancel) -> Option<PathBuf> {
    if !ffmpeg_available() {
        return None;
    }
    let mut cmd = Command::new(ffmpeg_sidecar::paths::ffmpeg_path());
    cmd.args([
        "-nostdin",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,crypto",
        "-ss",
        &format!("{:.3}", at_ms as f64 / 1000.0),
        "-i",
    ])
    .arg(path)
    .args(["-frames:v", "1", "-f", "image2pipe", "-c:v", "png", "-"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let png = run_capture(cmd, cancel)?;
    if png.is_empty() {
        return None;
    }
    let dir = dirs::cache_dir()?.join("piku").join("screenshots");
    std::fs::create_dir_all(&dir).ok()?;
    // The stem comes from a user file and lands in a filename we create.
    // Restrict it to a conservative set rather than trusting it: a name with
    // a separator, a reserved device name, or a bidi override has no business
    // shaping a path we write to.
    let stem = safe_stem(path);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    let dest = dir.join(format!("{stem}-{ts}.png"));
    // `create_new`: never truncate, and never follow a symlink someone
    // pre-planted at the destination. Owner-only, since a frame from a private
    // video is private.
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&dest).ok()?;
    std::io::Write::write_all(&mut file, &png).ok()?;
    prune_screenshots(&dir);
    Some(dest)
}

/// A filename stem safe to build a path from: ASCII alphanumerics, `.`, `-`,
/// and `_`, capped, never empty, never a reserved device name.
fn safe_stem(path: &Path) -> String {
    let raw = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(SAFE_STEM_CAP)
        .collect();
    let trimmed = cleaned.trim_matches(['.', '_', '-']).to_string();
    if trimmed.is_empty() || crate::security::path_guard::is_reserved_name(&trimmed) {
        "frame".to_string()
    } else {
        trimmed
    }
}

/// Keep the screenshot cache bounded. Nothing else ever deletes from it, so
/// without this it grows for the life of the installation.
fn prune_screenshots(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, e.path()))
        })
        .collect();
    if files.len() <= SCREENSHOT_KEEP {
        return;
    }
    // Oldest first, drop the excess.
    files.sort_by_key(|(mtime, _)| *mtime);
    for (_, path) in files.iter().take(files.len() - SCREENSHOT_KEEP) {
        let _ = std::fs::remove_file(path);
    }
}

/// Peak waveform (0..1, normalized, `buckets` bars) decoded via the bundled
/// ffmpeg — the fallback for formats `rodio` cannot decode. ffmpeg downmixes to
/// 8 kHz mono `s16le` on stdout (tiny: ~16 KB/s), which we bucket by abs-max.
/// Returns `None` when ffmpeg is unavailable or produced nothing. Blocking —
/// background executor only; same hardening as [`poster_frame`].
pub fn audio_pcm_peaks(path: &Path, buckets: usize, cancel: &Cancel) -> Option<Vec<f32>> {
    if !ffmpeg_available() || buckets == 0 {
        return None;
    }
    let mut cmd = Command::new(ffmpeg_sidecar::paths::ffmpeg_path());
    cmd.args([
        "-nostdin",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,crypto",
        "-i",
    ])
    .arg(path)
    .args(["-vn", "-ac", "1", "-ar", "8000", "-f", "s16le", "-"])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let pcm = run_capture(cmd, cancel)?;
    let n = pcm.len() / 2;
    if n == 0 {
        return None;
    }
    let mut peaks = vec![0f32; buckets];
    for (b, peak) in peaks.iter_mut().enumerate() {
        let lo = (b * n) / buckets;
        let hi = (((b + 1) * n) / buckets).max(lo + 1).min(n);
        let mut max = 0f32;
        for i in lo..hi {
            let s = i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]);
            let amp = (s as f32).abs() / 32768.0;
            if amp > max {
                max = amp;
            }
        }
        *peak = max;
    }
    let overall = peaks.iter().copied().fold(0.0f32, f32::max);
    if overall > 0.0 {
        for peak in peaks.iter_mut() {
            *peak /= overall;
        }
        Some(peaks)
    } else {
        None
    }
}

/// Spawn `cmd`, reading its stdout on a helper thread (so a full pipe never
/// deadlocks the wait) while enforcing [`POSTER_TIMEOUT`]; the child is killed
/// on timeout **or on cancellation**. Returns the captured bytes only on a
/// clean exit.
///
/// The cancellation check is the load-bearing one for two separate problems.
/// Arrowing through a folder of videos would otherwise leave one ffmpeg per
/// file running to completion. And [`POSTER_TIMEOUT`] is 15 s while the
/// runtime's shutdown grace is 100 ms, so without observing the shutdown token
/// here a quit during a poster extraction either blows through gpui's 200 ms
/// budget or orphans the subprocess.
fn run_capture(mut cmd: Command, cancel: &Cancel) -> Option<Vec<u8>> {
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
                if start.elapsed() > POSTER_TIMEOUT || cancel.is_cancelled() {
                    let _ = child.kill();
                    // Reap it: without the wait the child lingers as a zombie,
                    // and the reader thread below never sees its pipe close.
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
pub fn mp4_metadata(path: &Path) -> Option<Vec<MetaRow>> {
    let (file, total) = super::read::open_read(path).ok()?;
    if total == 0 {
        return None;
    }

    let parsed = catch_unwind(AssertUnwindSafe(|| {
        mp4::Mp4Reader::read_header(BufReader::new(file), total)
    }));
    let mp4 = parsed.ok()?.ok()?;

    let mut rows: Vec<MetaRow> = Vec::new();
    let secs = mp4.duration().as_secs();
    rows.push((
        "Duration".into(),
        format!("{}:{:02}", secs / 60, secs % 60).into(),
    ));

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
