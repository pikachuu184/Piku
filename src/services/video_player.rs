//! In-app, preview-grade video playback backed by the bundled ffmpeg.
//!
//! Design (honest about its limits): a background thread drives ffmpeg to decode
//! the video — from the current seek point — into scaled RGBA frames, which it
//! converts to gpui `RenderImage`s and pushes onto a **bounded** queue. The
//! UI-thread [`VideoPlayer`] runs a wall-clock playback clock; on each tick it
//! drains frames whose presentation time has passed and displays the latest, so
//! the render thread only ever blits an already-decoded image. Audio is
//! best-effort through rodio (silent if the container's audio can't be decoded);
//! A/V sync is approximate — this is preview playback, not a media engine.
//!
//! Everything expensive (decode, scale, pixel conversion) happens off the UI
//! thread. ffmpeg is spawned with `-nostdin`, `CREATE_NO_WINDOW`, and
//! `-protocol_whitelist file,crypto` so a crafted container can't make it reach
//! out to the network (SSRF); the frame queue is capped so memory stays bounded
//! regardless of video length or resolution.

use std::collections::VecDeque;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::FfmpegEvent;
use gpui::RenderImage;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};

use crate::preview::image_util::render_image_from_rgba;

/// Scale decoded frames down to at most this width (height auto, aspect kept).
/// Bounds decode work and per-frame memory for huge/hostile inputs while
/// staying crisp in the preview panel.
const PREVIEW_WIDTH: u32 = 960;

/// Max decoded frames buffered ahead of the playhead. At 960×540×4 ≈ 2 MiB per
/// frame this caps the queue near ~48 MiB no matter how long the video is.
const FRAME_QUEUE_CAP: usize = 24;

/// Default assumed frame rate until ffmpeg reports the real one (used only for
/// frame-stepping math).
const DEFAULT_FPS: f32 = 25.0;

struct DecodedFrame {
    pts_ms: u64,
    image: Arc<RenderImage>,
}

/// Shared between the UI-thread player and the current decode thread.
#[derive(Default)]
struct Shared {
    frames: Mutex<VecDeque<DecodedFrame>>,
    /// Frames-per-second × 1000 (0 until ffmpeg reports it).
    fps_milli: AtomicU64,
    /// Total duration in ms (0 until known).
    duration_ms: AtomicU64,
    /// Only frames tagged with the live generation are accepted; a seek bumps
    /// this so late frames from a superseded decode are dropped.
    generation: AtomicU64,
}

pub struct VideoPlayer {
    path: PathBuf,
    shared: Arc<Shared>,
    /// Stop flag for the *current* decode thread (replaced on every seek).
    stop: Arc<AtomicBool>,
    /// Seek point (ms) the current decode was launched at — added back to each
    /// frame's decode-relative timestamp.
    decode_base_ms: u64,

    playing: bool,
    /// Playback position (ms) captured whenever the clock last (re)started.
    base_ms: u64,
    clock_start: Option<Instant>,
    speed: f32,

    _stream: Option<(OutputStream, OutputStreamHandle)>,
    sink: Option<Sink>,
    volume: f32,
    muted: bool,

    current: Option<Arc<RenderImage>>,
}

impl VideoPlayer {
    /// Open `path` and begin buffering frames from the start (paused). Cheap:
    /// the decode runs on its own thread and fills only the bounded queue.
    pub fn new(path: PathBuf) -> Self {
        let shared = Arc::new(Shared::default());
        let (stream, sink) = build_audio(&path);
        let mut player = Self {
            path,
            shared,
            stop: Arc::new(AtomicBool::new(false)),
            decode_base_ms: 0,
            playing: false,
            base_ms: 0,
            clock_start: None,
            speed: 1.0,
            _stream: stream,
            sink,
            volume: 1.0,
            muted: false,
            current: None,
        };
        player.restart_decode(0);
        player
    }

    /// Tear down the current decode thread and launch a fresh one seeking to
    /// `at_ms`. The old thread sees its stop flag / a bumped generation and
    /// exits on its own (never joined, so the UI never blocks).
    fn restart_decode(&mut self, at_ms: u64) {
        self.stop.store(true, Ordering::Release);
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut q) = self.shared.frames.lock() {
            q.clear();
        }
        self.decode_base_ms = at_ms;
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let shared = self.shared.clone();
        let path = self.path.clone();
        std::thread::spawn(move || decode_loop(path, at_ms, generation, shared, stop));
    }

    pub fn fps(&self) -> f32 {
        let m = self.shared.fps_milli.load(Ordering::Acquire);
        if m > 0 {
            m as f32 / 1000.0
        } else {
            DEFAULT_FPS
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.shared.duration_ms.load(Ordering::Acquire)
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Current playback position (ms), advanced by the wall clock while playing
    /// and clamped to the known duration.
    pub fn position_ms(&self) -> u64 {
        let mut pos = self.base_ms;
        if self.playing
            && let Some(start) = self.clock_start
        {
            let elapsed = start.elapsed().as_secs_f32() * self.speed;
            pos = self.base_ms.saturating_add((elapsed * 1000.0) as u64);
        }
        let dur = self.duration_ms();
        if dur > 0 { pos.min(dur) } else { pos }
    }

    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        self.playing = true;
        self.clock_start = Some(Instant::now());
        if let Some(sink) = &self.sink {
            let _ = sink.try_seek(Duration::from_millis(self.base_ms));
            sink.set_speed(self.speed);
            sink.play();
        }
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        // Freeze the clock at the current position.
        self.base_ms = self.position_ms();
        self.playing = false;
        self.clock_start = None;
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }

    pub fn toggle(&mut self) {
        if self.playing {
            self.pause()
        } else {
            self.play()
        }
    }

    /// Seek to `at_ms`, restarting decode there and realigning audio. Preserves
    /// the play/pause state.
    pub fn seek(&mut self, at_ms: u64) {
        let dur = self.duration_ms();
        let at_ms = if dur > 0 { at_ms.min(dur) } else { at_ms };
        self.base_ms = at_ms;
        self.clock_start = self.playing.then(Instant::now);
        self.current = None;
        self.restart_decode(at_ms);
        if let Some(sink) = &self.sink {
            let _ = sink.try_seek(Duration::from_millis(at_ms));
        }
    }

    /// Step `frames` (negative = back) at the current fps, pausing first so the
    /// stepped frame stays on screen.
    pub fn step(&mut self, frames: i64) {
        self.pause();
        let frame_ms = (1000.0 / self.fps().max(1.0)) as i64;
        let target = (self.base_ms as i64 + frames * frame_ms).max(0) as u64;
        self.seek(target);
    }

    /// Cycle through a small set of playback speeds.
    pub fn cycle_speed(&mut self) {
        self.speed = match self.speed {
            s if s < 0.75 => 1.0,
            s if s < 1.25 => 1.5,
            s if s < 1.75 => 2.0,
            _ => 0.5,
        };
        // Re-anchor the clock so the new rate applies from here, not retroactively.
        self.base_ms = self.position_ms();
        self.clock_start = self.playing.then(Instant::now);
        if let Some(sink) = &self.sink {
            sink.set_speed(self.speed);
        }
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.muted = false;
        self.apply_volume();
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        self.apply_volume();
    }

    fn apply_volume(&self) {
        if let Some(sink) = &self.sink {
            sink.set_volume(if self.muted { 0.0 } else { self.volume });
        }
    }

    /// Advance to the frame for the current playback time and return it (or the
    /// last shown one). Called from the view's render/tick.
    pub fn current_frame(&mut self) -> Option<Arc<RenderImage>> {
        let pos = self.position_ms();
        if let Ok(mut q) = self.shared.frames.lock() {
            while let Some(front) = q.front() {
                if front.pts_ms <= pos {
                    // Drop everything already due, keeping the last as current.
                    let frame = q.pop_front();
                    if let Some(frame) = frame {
                        self.current = Some(frame.image);
                    }
                } else {
                    break;
                }
            }
        }
        self.current.clone()
    }

    /// Save a PNG of the frame at the current position into the app cache dir.
    /// Uses a fresh one-shot ffmpeg grab (decoupled from the display pipeline so
    /// it always works), returning the written path.
    pub fn screenshot(&self) -> Option<PathBuf> {
        crate::services::video_probe::save_frame_png(&self.path, self.position_ms())
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Signal the decode thread to exit; it kills its ffmpeg child.
        self.stop.store(true, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// Build a paused rodio sink for the file's audio track. Returns the stream
/// (kept alive for output) and the sink; either is `None` on failure (no audio
/// device, or the container's audio can't be decoded → silent playback).
fn build_audio(
    path: &std::path::Path,
) -> (Option<(OutputStream, OutputStreamHandle)>, Option<Sink>) {
    let Ok((file, _)) = crate::storage::local().open_read(path) else {
        return (None, None);
    };
    let Ok((stream, handle)) = OutputStream::try_default() else {
        return (None, None);
    };
    // rodio/symphonia can *panic* (not just error) while probing a track it
    // can't seek — common for video containers with no/odd audio. Catch it so a
    // video with unplayable audio plays silently instead of crashing the app.
    let sink = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let decoder = Decoder::new(BufReader::new(file)).ok()?;
        let sink = Sink::try_new(&handle).ok()?;
        sink.pause();
        sink.append(decoder);
        Some(sink)
    }))
    .ok()
    .flatten();
    (Some((stream, handle)), sink)
}

/// The decode thread body: drive ffmpeg, convert frames, push under backpressure.
fn decode_loop(
    path: PathBuf,
    seek_ms: u64,
    generation: u64,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) {
    let mut cmd = FfmpegCommand::new();
    cmd.create_no_window()
        .arg("-nostdin")
        // Block remote-URL fetches from a crafted container (SSRF); local only.
        .args(["-protocol_whitelist", "file,crypto"]);
    if seek_ms > 0 {
        cmd.seek(format!("{:.3}", seek_ms as f64 / 1000.0));
    }
    cmd.input(path.to_string_lossy())
        .arg("-an")
        .args(["-vf", &format!("scale='min({PREVIEW_WIDTH},iw)':-2")])
        .pix_fmt("rgba")
        .format("rawvideo")
        .arg("-");

    let Ok(mut child) = cmd.spawn() else {
        return;
    };
    let Ok(events) = child.iter() else {
        let _ = child.kill();
        return;
    };

    let superseded = |shared: &Shared| {
        stop.load(Ordering::Acquire) || shared.generation.load(Ordering::Acquire) != generation
    };

    for event in events {
        if superseded(&shared) {
            break;
        }
        match event {
            FfmpegEvent::ParsedInput(input) => {
                if let Some(d) = input.duration {
                    shared
                        .duration_ms
                        .store((d * 1000.0) as u64, Ordering::Release);
                }
            }
            FfmpegEvent::ParsedDuration(parsed) => {
                shared
                    .duration_ms
                    .store((parsed.duration * 1000.0) as u64, Ordering::Release);
            }
            FfmpegEvent::ParsedInputStream(stream) => {
                if let Some(video) = stream.video_data()
                    && video.fps > 0.0
                {
                    shared
                        .fps_milli
                        .store((video.fps * 1000.0) as u64, Ordering::Release);
                }
            }
            FfmpegEvent::OutputFrame(frame) => {
                let Some(rgba) = image::RgbaImage::from_raw(frame.width, frame.height, frame.data)
                else {
                    continue;
                };
                let pts_ms = seek_ms + (frame.timestamp.max(0.0) * 1000.0) as u64;
                let mut pending = Some(DecodedFrame {
                    pts_ms,
                    image: render_image_from_rgba(rgba),
                });
                // Backpressure: hold the frame until the queue drains below cap.
                while pending.is_some() {
                    if superseded(&shared) {
                        break;
                    }
                    if let Ok(mut q) = shared.frames.lock()
                        && q.len() < FRAME_QUEUE_CAP
                    {
                        q.push_back(pending.take().expect("pending is Some in this branch"));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            FfmpegEvent::Done | FfmpegEvent::LogEOF => break,
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: generate a real clip with the bundled ffmpeg, then confirm
    /// the decode thread → queue → frame pipeline delivers a frame and duration.
    /// Skips cleanly when ffmpeg isn't available (CI without the sidecar binary).
    #[test]
    fn decodes_frames_from_a_generated_clip() {
        if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let dir = std::env::temp_dir().join("piku-video-tests");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("testsrc.mp4");
        let made = std::process::Command::new(ffmpeg_sidecar::paths::ffmpeg_path())
            .args([
                "-y",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=10",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&clip)
            .status();
        if !matches!(made, Ok(status) if status.success()) {
            eprintln!("skipping: could not generate a test clip");
            return;
        }

        let mut player = VideoPlayer::new(clip);
        let mut got_frame = false;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            if player.current_frame().is_some() {
                got_frame = true;
                break;
            }
        }
        assert!(
            got_frame,
            "expected at least one decoded frame from the clip"
        );
        assert!(
            player.duration_ms() > 0,
            "expected ffmpeg to report a duration"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
