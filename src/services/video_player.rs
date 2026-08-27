//! In-app, preview-grade video playback backed by the bundled ffmpeg.
//!
//! Design (honest about its limits): a background thread drives ffmpeg to decode
//! the video — from the current seek point — into scaled **BGRA** frames at a
//! fixed [`TARGET_FPS`], which it wraps as gpui `RenderImage`s and pushes onto a
//! **bounded** queue. The UI-thread [`VideoPlayer`] runs a wall clock; on each
//! tick it drains frames whose presentation time has passed and displays the
//! latest, so the render thread only ever blits an already-decoded image.
//!
//! Three things here are load-bearing and were each a bug:
//!
//! * **Fixed output rate.** ffmpeg-sidecar does not expose real presentation
//!   timestamps — it derives them from the declared output frame rate. Forcing
//!   `fps=TARGET_FPS` in the filter graph makes the output genuinely
//!   constant-rate, so frame index *is* timestamp and this file computes it
//!   directly. Without it, variable-frame-rate sources (screen recordings, phone
//!   video, browser WebM) played visibly too fast or too slow.
//! * **BGRA out of ffmpeg.** gpui stores pixels as BGRA, so asking ffmpeg for
//!   `rgba` meant swapping half a million bytes per frame on the way in for no
//!   reason.
//! * **Audio through ffmpeg, not rodio.** See [`crate::services::ffmpeg_audio`]:
//!   rodio could not demux most video containers at all, which is why so many
//!   files played silently. Audio also anchors the clock — the wall clock stays
//!   the timeline (it can never stall) and the audio position corrects it.
//!
//! Everything expensive (decode, scale, pixel conversion) happens off the UI
//! thread. ffmpeg is spawned with `-nostdin`, `CREATE_NO_WINDOW`, a `file:`
//! input prefix, and `-protocol_whitelist file,crypto` so a crafted container
//! can't make it reach out to the network (SSRF); the frame queue is capped so
//! memory stays bounded regardless of video length or resolution.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ffmpeg_sidecar::command::FfmpegCommand;
use ffmpeg_sidecar::event::{FfmpegEvent, LogLevel};
use gpui::RenderImage;
use rodio::{OutputStream, OutputStreamHandle, Sink};

use crate::preview::image_util::render_image_from_bgra_bytes_checked;
use crate::services::ffmpeg_audio::FfmpegPcm;

/// Scale decoded frames down to at most this width (height auto, aspect kept).
/// Bounds decode work and per-frame memory for huge/hostile inputs while
/// staying crisp in the preview panel.
const PREVIEW_WIDTH: u32 = 960;

/// Frames per second asked of ffmpeg, and therefore the presentation rate.
///
/// Pinning this is what makes timestamps correct: the sidecar synthesizes them
/// from the output rate, so a constant-rate output means frame index maps exactly
/// to media time. It also caps pipe throughput on 60 fps sources. The cost is
/// that a 24 fps source has frames duplicated up to 30, which is cheap and
/// invisible next to getting the rate wrong.
const TARGET_FPS: u32 = 30;

/// Max decoded frames buffered ahead of the playhead. At 960×540×4 ≈ 2 MiB per
/// frame this caps the queue near ~48 MiB no matter how long the video is.
const FRAME_QUEUE_CAP: usize = 24;

/// Default assumed frame rate until ffmpeg reports the real one (used only for
/// frame-stepping math).
const DEFAULT_FPS: f32 = 25.0;

/// How far the wall clock may drift from the audio before it is re-anchored.
///
/// Large enough that ordinary jitter never triggers a correction, small enough
/// that lip sync never visibly slips.
const AV_RESYNC_MS: u64 = 150;

/// Minimum gap between decode restarts while scrubbing.
///
/// Dragging the scrubber fires a mouse-move per frame, and each one used to tear
/// down and respawn ffmpeg — a process spawn per event, which is a large part of
/// why dragging felt terrible. Intermediate positions still update the clock and
/// the scrubber immediately; only the respawn is coalesced.
const SEEK_COALESCE: Duration = Duration::from_millis(120);

/// A position past which an audio feed anchored at zero is worth respawning.
/// Below it the seek is inside the startup buffer and not worth a new process.
const AUDIO_REANCHOR_MS: u64 = 250;

/// Wait between retries while the frame queue is full.
const BACKPRESSURE_NAP: Duration = Duration::from_millis(5);

/// Cap on a stored ffmpeg error message. It is untrusted output headed for a UI
/// label, so it is sanitized and bounded before it is kept.
const ERROR_CAP: usize = 300;

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
    /// Frames handed to the queue by the current decode. Also the signal for
    /// whether an ffmpeg error is worth showing: one that did not stop playback
    /// is noise.
    frames_out: AtomicU64,
    /// Set once ffmpeg has finished the current decode without being superseded.
    eof: AtomicBool,
    /// First error ffmpeg reported, sanitized for display.
    error: Mutex<Option<String>>,
}

/// The live audio feed's clock: where it was started and how much real audio it
/// has delivered since.
struct AudioFeed {
    /// Real samples delivered to the mixer, published by [`FfmpegPcm`].
    played: Arc<AtomicU64>,
    /// Media position the feed was spawned at.
    base_ms: u64,
    /// Last position read, so a feed that is *not* advancing (a post-seek
    /// process spawn, an underrun) is never mistaken for a valid clock.
    last_ms: u64,
}

pub struct VideoPlayer {
    path: PathBuf,
    shared: Arc<Shared>,
    /// Stop flag for the *current* decode thread (replaced on every seek).
    stop: Arc<AtomicBool>,

    playing: bool,
    /// Playback position (ms) captured whenever the clock last (re)started.
    base_ms: u64,
    clock_start: Option<Instant>,
    speed: f32,
    /// When the clip was opened, for distinguishing "still starting up" from
    /// "this will never decode".
    opened_at: Instant,

    /// A scrub position not yet handed to the decoders; see [`SEEK_COALESCE`].
    pending_seek: Option<u64>,
    last_restart: Option<Instant>,

    _stream: Option<(OutputStream, OutputStreamHandle)>,
    sink: Option<Sink>,
    /// `Some` once a decodable audio track has been found; the clock uses it.
    audio: Option<AudioFeed>,
    volume: f32,
    muted: bool,

    current: Option<Arc<RenderImage>>,
}

impl VideoPlayer {
    /// Open `path` and begin buffering frames from the start (paused). Cheap:
    /// the decode runs on its own thread and fills only the bounded queue.
    pub fn new(path: PathBuf) -> Self {
        let mut player = Self {
            path,
            shared: Arc::new(Shared::default()),
            stop: Arc::new(AtomicBool::new(false)),
            playing: false,
            base_ms: 0,
            clock_start: None,
            speed: 1.0,
            opened_at: Instant::now(),
            pending_seek: None,
            last_restart: None,
            // No audio yet. Spawning ffmpeg and waiting for its first samples is
            // blocking work, and this runs inside `cx.new` on the UI thread;
            // `attach_audio` finishes the job once a track arrives from a worker.
            _stream: None,
            sink: None,
            audio: None,
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
        // Per-decode state: the new pass owns these outcomes, not the old one.
        self.shared.frames_out.store(0, Ordering::Release);
        self.shared.eof.store(false, Ordering::Release);
        if let Ok(mut error) = self.shared.error.lock() {
            *error = None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = stop.clone();
        let shared = self.shared.clone();
        let path = self.path.clone();
        std::thread::spawn(move || decode_loop(path, at_ms, generation, shared, stop));
    }

    /// Effective presentation rate: the source rate, capped at [`TARGET_FPS`]
    /// because that is what actually reaches the queue. Used for frame stepping.
    pub fn fps(&self) -> f32 {
        let m = self.shared.fps_milli.load(Ordering::Acquire);
        let source = if m > 0 {
            m as f32 / 1000.0
        } else {
            DEFAULT_FPS
        };
        source.min(TARGET_FPS as f32)
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

    /// A message to show in place of the frame surface, or `None` while things
    /// are merely still starting up.
    ///
    /// Only reported when nothing has decoded: ffmpeg logs recoverable stream
    /// complaints at error level on files that play perfectly, so an error that
    /// did not stop playback is noise, not news.
    pub fn error(&self) -> Option<String> {
        if self.shared.frames_out.load(Ordering::Acquire) > 0 {
            return None;
        }
        if let Ok(error) = self.shared.error.lock()
            && let Some(message) = error.as_deref()
        {
            return Some(message.to_string());
        }
        // ffmpeg exited without producing anything and without saying why.
        if self.shared.eof.load(Ordering::Acquire) {
            return Some("No video could be decoded from this file.".to_string());
        }
        None
    }

    /// How long the player has been trying to produce its first frame, for a
    /// view that wants to stop saying "Loading…" eventually.
    pub fn startup_elapsed(&self) -> Duration {
        self.opened_at.elapsed()
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
            // No `try_seek` here: the sink resumes where it paused, and asking
            // for a seek is what used to desync audio permanently on the
            // formats whose decoders return `NotSupported`.
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

    /// Seek to `at_ms` and restart the decoders now. Preserves play/pause.
    pub fn seek(&mut self, at_ms: u64) {
        self.mark_seek(at_ms);
        self.apply_seek();
    }

    /// Seek to `at_ms` for a scrubber drag: the clock and scrubber move at once,
    /// but the decoder restart is coalesced (see [`SEEK_COALESCE`]) so a drag
    /// cannot spawn an ffmpeg pair per mouse-move event.
    pub fn scrub(&mut self, at_ms: u64) {
        self.mark_seek(at_ms);
        self.flush_pending_seek();
    }

    /// Move the clock to `at_ms` and record that the decoders are now stale.
    fn mark_seek(&mut self, at_ms: u64) {
        let dur = self.duration_ms();
        let at_ms = if dur > 0 { at_ms.min(dur) } else { at_ms };
        self.base_ms = at_ms;
        self.clock_start = self.playing.then(Instant::now);
        // The last frame stays on screen until a new one arrives — blanking the
        // surface on every mouse-move made dragging flicker.
        if let Some(feed) = &mut self.audio {
            feed.base_ms = at_ms;
            feed.last_ms = at_ms;
        }
        self.pending_seek = Some(at_ms);
    }

    /// Restart the decoders at the pending position once the coalescing window
    /// has passed. Called from the render tick as well, so a drag that stops
    /// moving always lands.
    fn flush_pending_seek(&mut self) {
        if self.pending_seek.is_none() {
            return;
        }
        if let Some(last) = self.last_restart
            && last.elapsed() < SEEK_COALESCE
        {
            return;
        }
        self.apply_seek();
    }

    fn apply_seek(&mut self) {
        let Some(at_ms) = self.pending_seek.take() else {
            return;
        };
        self.last_restart = Some(Instant::now());
        self.restart_decode(at_ms);
        self.restart_audio(at_ms);
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
        self.flush_pending_seek();
        self.resync_to_audio();
        self.settle_at_eof();
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

    /// Pull the wall clock onto the audio position when the two disagree.
    ///
    /// The wall clock stays the timeline on purpose: it can never stall, which a
    /// sample-counting clock can (an ffmpeg spawn after a seek, an underrun on a
    /// slow disk). Audio only *corrects* it, and only while it is demonstrably
    /// advancing, so a stalled feed can never drag playback backwards.
    fn resync_to_audio(&mut self) {
        if !self.playing || self.pending_seek.is_some() {
            return;
        }
        let wall = self.position_ms();
        let Some(feed) = self.audio.as_mut() else {
            return;
        };
        let played = FfmpegPcm::played_ms(feed.played.load(Ordering::Acquire));
        let audio_ms = feed.base_ms.saturating_add(played);
        let advancing = audio_ms > feed.last_ms;
        feed.last_ms = audio_ms;
        if advancing && wall.abs_diff(audio_ms) > AV_RESYNC_MS {
            self.base_ms = audio_ms;
            self.clock_start = Some(Instant::now());
        }
    }

    /// Stop the clock at the end of the clip instead of letting it run past the
    /// end with the ticker spinning forever.
    fn settle_at_eof(&mut self) {
        if !self.playing || self.pending_seek.is_some() || !self.shared.eof.load(Ordering::Acquire)
        {
            return;
        }
        let drained = self
            .shared
            .frames
            .lock()
            .map(|q| q.is_empty())
            .unwrap_or(false);
        let dur = self.duration_ms();
        if !drained || dur == 0 || self.position_ms() < dur {
            return;
        }
        self.base_ms = dur;
        self.playing = false;
        self.clock_start = None;
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }

    /// Attach an audio track built off-thread.
    ///
    /// Must run on the thread that owns the player: `OutputStream` is `!Send`,
    /// so it is created here rather than travelling with the track.
    pub fn attach_audio(&mut self, track: AudioTrack) {
        let Ok((stream, handle)) = OutputStream::try_default() else {
            return;
        };
        let played = track.played();
        let Some(sink) = build_sink(&handle, track) else {
            return;
        };
        // Match whatever the user has already set while the track was loading.
        sink.set_volume(if self.muted { 0.0 } else { self.volume });
        sink.set_speed(self.speed);
        if self.playing {
            sink.play();
        }
        self._stream = Some((stream, handle));
        self.sink = Some(sink);
        self.audio = Some(AudioFeed {
            played,
            base_ms: 0,
            last_ms: 0,
        });
        // A seek while the track was loading left the player elsewhere; the feed
        // is anchored at zero, so re-anchor now that there is one to move.
        let pos = self.position_ms();
        if pos > AUDIO_REANCHOR_MS {
            self.restart_audio(pos);
        }
    }

    /// Point the audio feed at `at_ms` by respawning ffmpeg there.
    ///
    /// Seeking is a fresh process rather than `Sink::try_seek` because symphonia
    /// returns `NotSupported` for several of these formats, which left audio
    /// stuck at its old position for the rest of the session. A no-op until a
    /// decodable track has been found, so a silent file stays silent.
    fn restart_audio(&mut self, at_ms: u64) {
        if self.audio.is_none() {
            return;
        }
        let Some((_, handle)) = self._stream.as_ref() else {
            return;
        };
        // `resume` rather than `open`: this may run on the UI thread, and the
        // file is already known to have audio, so there is nothing to wait for.
        let Some(track) = FfmpegPcm::resume(&self.path, at_ms) else {
            return;
        };
        let played = track.played();
        // A fresh sink rather than `Sink::clear()`: clear blocks the caller until
        // the mixer acknowledges it, and leaves the sink paused.
        let Some(sink) = build_sink(handle, track) else {
            return;
        };
        sink.set_volume(if self.muted { 0.0 } else { self.volume });
        sink.set_speed(self.speed);
        if self.playing {
            sink.play();
        }
        if let Some(old) = self.sink.replace(sink) {
            old.stop();
        }
        self.audio = Some(AudioFeed {
            played,
            base_ms: at_ms,
            last_ms: at_ms,
        });
    }

    /// The file this player is showing, for a screenshot request.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        // Signal the decode thread to exit; it kills its ffmpeg child.
        self.stop.store(true, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        // The sink's source chain owns the audio child; stopping it first means
        // the mixer drops the source (and kills ffmpeg) promptly.
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
    }
}

/// Build a paused sink for `track`.
///
/// rodio can *panic* rather than error while wiring up a source, so the whole
/// construction is wrapped: an audio failure must leave a silent video, not take
/// the app down.
fn build_sink(handle: &OutputStreamHandle, track: AudioTrack) -> Option<Sink> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sink = Sink::try_new(handle).ok()?;
        sink.pause();
        sink.append(track);
        Some(sink)
    }))
    .ok()
    .flatten()
}

/// The audio track, ready to hand to a sink. `Send`, which is the whole point of
/// the split between [`open_audio_track`] and [`VideoPlayer::attach_audio`].
pub type AudioTrack = FfmpegPcm;

/// Find and open the file's audio. **Blocking — worker threads only.**
///
/// This is the half of building audio that can move off the UI thread. The other
/// half cannot: `rodio::OutputStream` is `!Send`, so it must be created on the
/// thread that will hold it, which is why [`VideoPlayer::attach_audio`] exists
/// rather than this simply returning a `Sink`.
///
/// Returns `None` when the file has no audio ffmpeg can decode, which is the
/// signal the player uses to keep the wall clock as its only timeline.
pub fn open_audio_track(path: &std::path::Path) -> Option<AudioTrack> {
    FfmpegPcm::open(path, 0)
}

/// The decode thread body.
///
/// Runs ffmpeg once with hardware decoding and, if that produced nothing at all,
/// once more in software. `-hwaccel auto` is documented to fall back on its own
/// when the method is merely unavailable, but a *broken* driver fails at decode
/// time — and an empty panel is a much worse outcome than a slower one.
fn decode_loop(
    path: PathBuf,
    seek_ms: u64,
    generation: u64,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) {
    let superseded =
        || stop.load(Ordering::Acquire) || shared.generation.load(Ordering::Acquire) != generation;

    let frames = run_ffmpeg(&path, seek_ms, &shared, &superseded, true);
    if frames == 0 && !superseded() {
        // Clear the hardware attempt's complaint so the software pass reports
        // its own, if any.
        if let Ok(mut error) = shared.error.lock() {
            *error = None;
        }
        run_ffmpeg(&path, seek_ms, &shared, &superseded, false);
    }
    if !superseded() {
        shared.eof.store(true, Ordering::Release);
    }
}

/// One ffmpeg pass. Returns the number of frames pushed to the queue.
fn run_ffmpeg(
    path: &Path,
    seek_ms: u64,
    shared: &Shared,
    superseded: &dyn Fn() -> bool,
    hwaccel: bool,
) -> u64 {
    let mut cmd = FfmpegCommand::new();
    cmd.arg("-nostdin")
        // Block remote-URL fetches from a crafted container (SSRF); local only.
        .args(["-protocol_whitelist", "file,crypto"]);
    if hwaccel {
        cmd.hwaccel("auto");
    }
    if seek_ms > 0 {
        // Before `-i`: input seeking, which skips rather than decodes.
        cmd.seek(format!("{:.3}", seek_ms as f64 / 1000.0));
    }
    cmd.arg("-i")
        .arg(input_arg(path))
        // Exactly one stream out. A container with cover art or an attachment
        // otherwise risks confusing the sidecar's output parsing, which falls
        // back to a chunked mode this loop cannot use.
        .args(["-map", "0:v:0", "-an", "-sn", "-dn"])
        .args([
            "-vf",
            &format!("scale='min({PREVIEW_WIDTH},iw)':-2:flags=bilinear,fps={TARGET_FPS}"),
        ])
        // gpui's native pixel order, so nothing has to be swapped on the way in.
        .pix_fmt("bgra")
        .format("rawvideo")
        .arg("-");
    // Deliberately no `-loglevel` override: `FfmpegCommand` already sets
    // `level+info`, and its parser reads fps, duration, and stream metadata out
    // of stderr. Quieting it would starve the metadata this player needs.

    let Ok(mut child) = cmd.spawn() else {
        record_error(shared, "Could not start the video decoder.");
        return 0;
    };
    let Ok(events) = child.iter() else {
        let _ = child.kill();
        let _ = child.wait();
        record_error(shared, "Could not read from the video decoder.");
        return 0;
    };

    // Presentation timestamps are computed here rather than taken from the
    // sidecar: it derives them from the parsed output frame rate, and `fps=` in
    // the filter graph above already guarantees a constant rate, so the frame
    // index is the exact answer.
    let mut index: u64 = 0;
    for event in events {
        if superseded() {
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
                // ffmpeg was asked for `bgra`, which is gpui's own order, so this
                // is a move — no per-pixel work anywhere on this path.
                let Some(image) =
                    render_image_from_bgra_bytes_checked(frame.width, frame.height, frame.data)
                else {
                    continue; // A truncated frame: skip it rather than show noise.
                };
                let pts_ms = seek_ms + (index * 1000) / TARGET_FPS as u64;
                index += 1;
                if push_frame(shared, superseded, DecodedFrame { pts_ms, image }) {
                    shared.frames_out.fetch_add(1, Ordering::Release);
                }
            }
            // Unparsed output: the sidecar could not determine the frame layout
            // and fell back to raw chunks, which this loop cannot assemble.
            // Silence here is what used to look like a permanent "Loading…".
            FfmpegEvent::OutputChunk(_) => {
                record_error(shared, "This video's format could not be read.");
                break;
            }
            FfmpegEvent::Error(message) | FfmpegEvent::Log(LogLevel::Error, message) => {
                record_error(shared, &message);
            }
            // Only `Done` ends the pass. `LogEOF` means stderr closed, and it
            // shares a rendezvous channel with the frame stream — breaking on it
            // raced the tail frames and truncated the end of every clip.
            FfmpegEvent::Done => break,
            _ => {}
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    index
}

/// Hold `frame` until the queue drains below cap. Returns whether it landed.
///
/// The lock is released before each sleep — the assignment ends the statement the
/// guard's temporary belongs to.
fn push_frame(shared: &Shared, superseded: &dyn Fn() -> bool, frame: DecodedFrame) -> bool {
    let mut pending = Some(frame);
    while let Some(frame) = pending.take() {
        if superseded() {
            return false;
        }
        pending = match shared.frames.lock() {
            Ok(mut q) if q.len() < FRAME_QUEUE_CAP => {
                q.push_back(frame);
                None
            }
            _ => Some(frame),
        };
        if pending.is_some() {
            std::thread::sleep(BACKPRESSURE_NAP);
        }
    }
    true
}

/// Keep the first thing ffmpeg complained about, sanitized.
///
/// The text is subprocess output shaped by an untrusted file and it is headed
/// for a UI label, so it goes through the same sanitizer as filenames and git
/// output — a bidi override in a stream title has no business reordering a
/// message in the player.
fn record_error(shared: &Shared, message: &str) {
    let cleaned = crate::security::text::sanitize_display(message.trim(), ERROR_CAP, false);
    if cleaned.is_empty() {
        return;
    }
    if let Ok(mut error) = shared.error.lock()
        && error.is_none()
    {
        *error = Some(cleaned);
    }
}

/// Prefix the input with `file:` so ffmpeg's protocol is pinned even if the path
/// itself looks like a URL — defence in depth beside `-protocol_whitelist`.
///
/// Built as an `OsString`: `to_string_lossy` would replace a non-UTF-8 component
/// with U+FFFD and hand ffmpeg a path that does not exist, or a different one.
/// argv carries bytes, so there is no need to round-trip through UTF-8.
fn input_arg(path: &Path) -> OsString {
    let mut arg = OsString::from("file:");
    arg.push(path.as_os_str());
    arg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("piku-video-tests");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Generate a clip with `args`, returning `None` when ffmpeg is unavailable
    /// or the encode failed (so tests skip rather than fail without the sidecar).
    fn generate(name: &str, args: &[&str]) -> Option<std::path::PathBuf> {
        if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
            return None;
        }
        let clip = scratch().join(name);
        let status = std::process::Command::new(ffmpeg_sidecar::paths::ffmpeg_path())
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(&clip)
            .status();
        matches!(status, Ok(status) if status.success()).then_some(clip)
    }

    /// Wait up to ~2s for the pipeline to deliver a frame.
    fn wait_for_frame(player: &mut VideoPlayer) -> bool {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(50));
            if player.current_frame().is_some() {
                return true;
            }
        }
        false
    }

    /// End-to-end: generate a real clip with the bundled ffmpeg, then confirm
    /// the decode thread → queue → frame pipeline delivers a frame and duration.
    /// Skips cleanly when ffmpeg isn't available (CI without the sidecar binary).
    #[test]
    fn decodes_frames_from_a_generated_clip() {
        let Some(clip) = generate(
            "testsrc.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=10",
                "-pix_fmt",
                "yuv420p",
            ],
        ) else {
            eprintln!("skipping: could not generate a test clip");
            return;
        };

        let mut player = VideoPlayer::new(clip.clone());
        assert!(
            wait_for_frame(&mut player),
            "expected at least one decoded frame from the clip"
        );
        assert!(
            player.duration_ms() > 0,
            "expected ffmpeg to report a duration"
        );
        assert!(
            player.error().is_none(),
            "a clip that decoded should report no error"
        );
        let _ = std::fs::remove_file(&clip);
    }

    /// The timing fix: a variable-frame-rate source must still present frames on
    /// the fixed [`TARGET_FPS`] grid. Before the `fps=` filter, timestamps came
    /// from the source's declared rate and VFR clips played at the wrong speed.
    #[test]
    fn a_variable_frame_rate_clip_lands_on_the_target_grid() {
        // `-vsync vfr` with a rate-changing filter yields genuinely uneven
        // source timestamps.
        let Some(clip) = generate(
            "vfr.mkv",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=2:size=160x120:rate=50",
                "-vf",
                "fps=fps=50,select='not(mod(n,3))'",
                "-fps_mode",
                "vfr",
                "-pix_fmt",
                "yuv420p",
            ],
        ) else {
            eprintln!("skipping: could not generate a VFR clip");
            return;
        };

        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        // Decode on its own thread and drain as it goes. `push_frame` parks the
        // decoder while the queue is full — the backpressure that bounds the
        // player's memory — so collecting only after `decode_loop` returned
        // would deadlock at frame `FRAME_QUEUE_CAP` and hang the test run.
        let worker = {
            let (clip, shared, stop) = (clip.clone(), shared.clone(), stop.clone());
            std::thread::spawn(move || decode_loop(clip, 0, 0, shared, stop))
        };
        let mut queued: Vec<u64> = Vec::new();
        let drain = |queued: &mut Vec<u64>| {
            if let Ok(mut q) = shared.frames.lock() {
                queued.extend(q.drain(..).map(|f| f.pts_ms));
            }
        };
        let deadline = Instant::now() + Duration::from_secs(60);
        while !shared.eof.load(Ordering::Acquire) && Instant::now() < deadline {
            drain(&mut queued);
            std::thread::sleep(Duration::from_millis(10));
        }
        // Whatever landed between the last sweep and EOF.
        drain(&mut queued);
        stop.store(true, Ordering::Release);
        let _ = worker.join();

        assert!(!queued.is_empty(), "no frames decoded from the VFR clip");
        let step = 1000 / TARGET_FPS as u64;
        for (n, pts) in queued.iter().enumerate() {
            // Multiply before dividing, exactly as the decoder does: a per-frame
            // `n * (1000 / 30)` truncates 0.33 ms each time and drifts a full
            // second across half a minute of video.
            assert_eq!(
                *pts,
                n as u64 * 1000 / TARGET_FPS as u64,
                "frame {n} is off the {TARGET_FPS} fps grid (~{step}ms apart)"
            );
        }
        // The source keeps every third frame of 50 fps — about 34 frames across
        // two seconds. Landing near 2 s × `TARGET_FPS` is what proves the filter
        // graph re-timed it to a constant rate instead of passing the source's
        // uneven timestamps through, which is what made VFR clips play slow.
        assert!(
            queued.len() >= 50,
            "expected ~{} frames after CFR conversion, got {}",
            2 * TARGET_FPS,
            queued.len()
        );
        let _ = std::fs::remove_file(&clip);
    }

    /// A file ffmpeg cannot decode must produce a message, not an eternal
    /// "Loading…" — the symptom that made a failed decode indistinguishable
    /// from a slow one.
    #[test]
    fn an_undecodable_file_reports_an_error() {
        if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let junk = scratch().join("notes.txt");
        if std::fs::write(&junk, b"this is not a video").is_err() {
            eprintln!("skipping: could not write the test file");
            return;
        }
        let mut player = VideoPlayer::new(junk.clone());
        let mut message = None;
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(50));
            message = player.error();
            if message.is_some() {
                break;
            }
        }
        // Also confirm the surface never gets a frame to show, which is the
        // other half of "an error, not an eternal Loading…".
        assert!(player.current_frame().is_none());
        assert!(
            message.is_some(),
            "an undecodable file must report an error rather than buffer forever"
        );
        let _ = std::fs::remove_file(&junk);
    }

    /// Dragging the scrubber must not respawn the decoders per mouse-move.
    #[test]
    fn scrubbing_coalesces_decoder_restarts() {
        let mut player = VideoPlayer {
            path: PathBuf::from("/nonexistent.mp4"),
            shared: Arc::new(Shared::default()),
            stop: Arc::new(AtomicBool::new(false)),
            playing: false,
            base_ms: 0,
            clock_start: None,
            speed: 1.0,
            opened_at: Instant::now(),
            pending_seek: None,
            // Pretend a restart just happened, so the coalescing window is open.
            last_restart: Some(Instant::now()),
            _stream: None,
            sink: None,
            audio: None,
            volume: 1.0,
            muted: false,
            current: None,
        };
        let before = player.shared.generation.load(Ordering::Acquire);
        for at in [1_000, 1_100, 1_200, 1_300] {
            player.scrub(at);
        }
        assert_eq!(
            player.shared.generation.load(Ordering::Acquire),
            before,
            "a drag inside the coalescing window restarted the decoder"
        );
        assert_eq!(
            player.pending_seek,
            Some(1_300),
            "the newest scrub position must survive to be applied"
        );
        // The clock still tracks the drag, so the scrubber does not lag.
        assert_eq!(player.position_ms(), 1_300);

        // An explicit seek is immediate: clicks and step buttons must not wait.
        player.seek(2_000);
        assert!(player.pending_seek.is_none());
        assert!(player.shared.generation.load(Ordering::Acquire) > before);
    }

    /// The audio clock corrects the wall clock, but only while it is actually
    /// advancing — a stalled feed must never drag playback backwards.
    #[test]
    fn a_stalled_audio_feed_does_not_rewind_the_clock() {
        let played = Arc::new(AtomicU64::new(0));
        let mut player = VideoPlayer {
            path: PathBuf::from("/nonexistent.mp4"),
            shared: Arc::new(Shared::default()),
            stop: Arc::new(AtomicBool::new(false)),
            playing: true,
            base_ms: 5_000,
            clock_start: Some(Instant::now()),
            speed: 1.0,
            opened_at: Instant::now(),
            pending_seek: None,
            last_restart: None,
            _stream: None,
            sink: None,
            audio: Some(AudioFeed {
                played: played.clone(),
                // Anchored far behind: a naive resync would jump here.
                base_ms: 0,
                last_ms: 0,
            }),
            volume: 1.0,
            muted: false,
            current: None,
        };
        player.resync_to_audio();
        assert_eq!(
            player.base_ms, 5_000,
            "a feed that has delivered nothing must not re-anchor the clock"
        );

        // Now it advances, and far enough out to warrant a correction.
        played.store(
            FfmpegPcm::samples_per_second() * 2, // 2 s of audio
            Ordering::Release,
        );
        player.resync_to_audio();
        assert!(
            player.base_ms.abs_diff(2_000) < 50,
            "an advancing feed must pull the clock onto it, got {}",
            player.base_ms
        );
    }
}
