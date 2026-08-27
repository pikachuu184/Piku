//! Audio playback backend for the preview panel.
//!
//! Holds a rodio output stream + sink. rodio does its own mixing on a
//! dedicated background thread, so appending a decoded source never blocks the
//! UI thread. The stream/sink are `!Send`, which is fine: gpui entities live
//! on the single UI thread (`Entity` only requires `'static`). While audio is
//! playing a lightweight ticker re-renders the panel so the transport bar
//! advances.
//!
//! Two decoders, in order. rodio's own is the fast path and handles
//! MP3/FLAC/WAV/M4A directly from the file. What it cannot do is demux Matroska
//! or ASF, or decode Opus, AC-3, or HE-AAC at all — the symphonia features rodio
//! enables simply don't include them — so `.opus`, `.mka`, `.wma`, and `.ac3`
//! files used to be silent no-ops. Those fall through to
//! [`FfmpegPcm`](crate::services::ffmpeg_audio::FfmpegPcm), which streams PCM out
//! of the bundled ffmpeg through a bounded ring.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Context, SharedString};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};

use crate::services::ffmpeg_audio::FfmpegPcm;

/// Refuse to load an audio file larger than this for in-app playback — a
/// hostile or accidentally huge file should never be streamed into the
/// decoder. Metadata/waveform previews have their own (smaller) caps.
const MAX_PLAY_BYTES: u64 = 512 * 1024 * 1024;

/// Now-playing information the global player owns so any observer (the inspector
/// preview, the bottom bar, a docked media panel) can label the track without
/// re-reading tags.
#[derive(Clone, Default)]
pub struct TrackMeta {
    pub title: SharedString,
    pub artist: Option<SharedString>,
    /// Total length in ms; `0` when unknown (used to clamp seeks + draw progress).
    pub duration_ms: u64,
}

#[derive(Default)]
pub struct AudioPlayer {
    /// Kept alive for the lifetime of playback — dropping it silences output.
    stream: Option<(OutputStream, OutputStreamHandle)>,
    sink: Option<Sink>,
    current: Option<PathBuf>,
    meta: Option<TrackMeta>,
    /// Logical level 0..=1 (what the UI shows); the sink gets `effective_volume`.
    volume: f32,
    muted: bool,
    ticking: bool,
    /// Bumped on every `load`; an async sink build from a superseded load is
    /// discarded when it finally arrives.
    load_gen: u64,
    /// Set when the loaded track is being streamed through ffmpeg rather than
    /// decoded by rodio. Those sources cannot `try_seek`, so seeking respawns
    /// them; see [`AudioPlayer::seek`].
    streamed: bool,
    /// Media offset the current sink was started at. Non-zero only after an
    /// ffmpeg-backed seek, whose fresh sink reports `get_pos` from zero.
    sink_base: Duration,
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self {
            volume: 1.0,
            ..Self::default()
        }
    }

    fn handle(&mut self) -> Option<OutputStreamHandle> {
        if self.stream.is_none() {
            // Created lazily on first play so an audio-device failure never
            // affects the rest of the app.
            self.stream = OutputStream::try_default().ok();
        }
        self.stream.as_ref().map(|(_, handle)| handle.clone())
    }

    pub fn is_current(&self, path: &Path) -> bool {
        self.current.as_deref() == Some(path)
    }

    pub fn is_playing(&self) -> bool {
        self.sink
            .as_ref()
            .is_some_and(|s| !s.is_paused() && !s.empty())
    }

    pub fn position(&self) -> Duration {
        self.sink
            .as_ref()
            .map(|s| self.sink_base + s.get_pos())
            .unwrap_or(Duration::ZERO)
    }

    /// The currently loaded track's path, if any.
    pub fn current(&self) -> Option<&Path> {
        self.current.as_deref()
    }

    /// Now-playing metadata for the loaded track.
    pub fn meta(&self) -> Option<&TrackMeta> {
        self.meta.as_ref()
    }

    /// Loaded track length in ms (`0` when unknown).
    pub fn duration_ms(&self) -> u64 {
        self.meta.as_ref().map_or(0, |m| m.duration_ms)
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// What the sink actually plays at: silent while muted.
    fn effective_volume(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    /// Set the logical volume (clamped 0..=1); un-mutes.
    pub fn set_volume(&mut self, volume: f32, cx: &mut Context<Self>) {
        self.volume = clamp_volume(volume);
        self.muted = false;
        if let Some(sink) = &self.sink {
            sink.set_volume(self.effective_volume());
        }
        cx.notify();
    }

    pub fn toggle_mute(&mut self, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        if let Some(sink) = &self.sink {
            sink.set_volume(self.effective_volume());
        }
        cx.notify();
    }

    /// Play `path` from the start (recording now-playing metadata for
    /// observers), or toggle pause if it is already the loaded track.
    pub fn play_or_toggle_meta(
        &mut self,
        path: PathBuf,
        meta: Option<TrackMeta>,
        cx: &mut Context<Self>,
    ) {
        if self.is_current(&path) {
            self.toggle(cx);
        } else {
            self.load(path, meta, cx);
        }
    }

    fn load(&mut self, path: PathBuf, meta: Option<TrackMeta>, cx: &mut Context<Self>) {
        let Some(handle) = self.handle() else {
            return;
        };
        // Show now-playing immediately so the bottom bar/inspector update on the
        // click; the sink (open + decoder header probe) is built off the UI
        // thread so a large or slow file can't stall the interface.
        self.meta = Some(meta.unwrap_or_else(|| {
            TrackMeta {
                title: path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string()
                    .into(),
                ..TrackMeta::default()
            }
        }));
        self.current = Some(path.clone());
        if let Some(old) = self.sink.take() {
            old.stop();
        }
        self.sink_base = Duration::ZERO;
        self.load_gen += 1;
        let generation = self.load_gen;
        let volume = self.effective_volume();

        let build = cx.background_executor().spawn(async move {
            // Reads go through the sanitizing local provider (refuses symlinks
            // and directories). It gates *both* decoders: the ffmpeg fallback
            // below only runs once this has vouched for the path, so handing a
            // path to a subprocess never skips the check.
            let (file, total) = crate::storage::local().open_read(&path).ok()?;
            // Untrusted input: never stream an oversized file into rodio, which
            // reads the file itself and buffers as it sees fit.
            if total <= MAX_PLAY_BYTES
                && let Some(sink) = rodio_sink(file, &handle, volume)
            {
                return Some((sink, false));
            }
            // rodio could not read it. ffmpeg streams through a bounded ring, so
            // the size cap above does not apply here — nothing is ever fully
            // buffered no matter how long the file is.
            ffmpeg_sink(&path, 0, &handle, volume).map(|sink| (sink, true))
        });
        cx.spawn(async move |this, cx| {
            let built = build.await;
            let _ = this.update(cx, |this, cx| {
                // A newer load superseded this one — drop the stale sink.
                if this.load_gen != generation {
                    if let Some((sink, _)) = built {
                        sink.stop();
                    }
                    return;
                }
                if let Some((sink, streamed)) = built {
                    sink.play();
                    this.sink = Some(sink);
                    this.streamed = streamed;
                    this.start_ticker(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub fn toggle(&mut self, cx: &mut Context<Self>) {
        let resume = match &self.sink {
            Some(sink) if sink.is_paused() => {
                sink.play();
                true
            }
            Some(sink) => {
                sink.pause();
                false
            }
            None => false,
        };
        if resume {
            self.start_ticker(cx);
        }
        cx.notify();
    }

    pub fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(sink) = self.sink.take() {
            sink.stop();
        }
        self.current = None;
        self.meta = None;
        self.streamed = false;
        self.sink_base = Duration::ZERO;
        cx.notify();
    }

    pub fn seek(&mut self, pos: Duration, cx: &mut Context<Self>) {
        // Clamp into the known track length so a scrubber drag past the end
        // can't hand the decoder an out-of-range position.
        let target = clamp_seek(pos, self.duration_ms());
        if self.streamed {
            // An ffmpeg-backed source has no `try_seek` to offer, so the seek is
            // a respawn at the new offset — the same trade the video player
            // makes. Without this the scrubber would silently snap back on
            // exactly the formats this fallback exists to play.
            self.restart_stream(target, cx);
        } else if let Some(sink) = &self.sink {
            let _ = sink.try_seek(target);
        }
        cx.notify();
    }

    /// Rebuild the ffmpeg-backed sink at `at`, preserving play/pause.
    ///
    /// A fresh sink rather than `Sink::clear()`: clear blocks the caller until
    /// the mixer acknowledges it, and this runs on the UI thread.
    fn restart_stream(&mut self, at: Duration, cx: &mut Context<Self>) {
        let Some(path) = self.current.clone() else {
            return;
        };
        let Some(handle) = self.handle() else {
            return;
        };
        let was_playing = self.is_playing();
        // A fresh sink starts unpaused, so match the old state explicitly.
        let Some(sink) = ffmpeg_sink(
            &path,
            at.as_millis() as u64,
            &handle,
            self.effective_volume(),
        ) else {
            return;
        };
        if !was_playing {
            sink.pause();
        }
        if let Some(old) = self.sink.replace(sink) {
            old.stop();
        }
        // `get_pos` on the new sink counts from zero; the transport reads
        // absolute media time.
        self.sink_base = at;
        if was_playing {
            self.start_ticker(cx);
        }
    }

    /// Re-render the panel every 250 ms while audio plays, so the transport bar
    /// advances. Exits (and clears the flag) as soon as playback stops or
    /// pauses; guarded so only one ticker ever runs.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let keep_going = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        if this.is_playing() {
                            true
                        } else {
                            this.ticking = false;
                            false
                        }
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }
}

/// Build a playing sink from rodio's own decoder — the fast path.
///
/// rodio/symphonia can *panic* (not just error) probing a malformed or
/// unsupported stream, so the whole construction is wrapped: a bad file must be
/// a no-op, not a crash. A panic here is also a legitimate fall-through to
/// ffmpeg, which is why this returns `Option` rather than reporting.
fn rodio_sink(file: std::fs::File, handle: &OutputStreamHandle, volume: f32) -> Option<Sink> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let decoder = Decoder::new(BufReader::new(file)).ok()?;
        let sink = Sink::try_new(handle).ok()?;
        sink.set_volume(volume);
        sink.append(decoder);
        Some(sink)
    }))
    .ok()
    .flatten()
}

/// Build a playing sink that streams PCM out of ffmpeg from `at_ms`.
///
/// `None` when ffmpeg is unavailable or the file has no audio it can decode —
/// the same graceful silence as before, but now only for files that genuinely
/// have nothing to play.
///
/// **Blocking on the load path** ([`FfmpegPcm::open`] waits for the first chunk,
/// which is how "no audio" is detected); the seek path uses
/// [`FfmpegPcm::resume`], which only spawns.
fn ffmpeg_sink(path: &Path, at_ms: u64, handle: &OutputStreamHandle, volume: f32) -> Option<Sink> {
    let track = if at_ms == 0 {
        FfmpegPcm::open(path, 0)?
    } else {
        FfmpegPcm::resume(path, at_ms)?
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let sink = Sink::try_new(handle).ok()?;
        sink.set_volume(volume);
        sink.append(track);
        Some(sink)
    }))
    .ok()
    .flatten()
}

/// Clamp a requested volume into the sink's valid 0..=1 range.
fn clamp_volume(volume: f32) -> f32 {
    volume.clamp(0.0, 1.0)
}

/// Clamp a seek target into `[0, duration]`; an unknown duration (`0`) leaves
/// the position untouched.
fn clamp_seek(pos: Duration, duration_ms: u64) -> Duration {
    if duration_ms > 0 {
        pos.min(Duration::from_millis(duration_ms))
    } else {
        pos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_is_clamped() {
        assert_eq!(clamp_volume(-1.0), 0.0);
        assert_eq!(clamp_volume(0.5), 0.5);
        assert_eq!(clamp_volume(3.0), 1.0);
    }

    #[test]
    fn seek_clamps_to_duration() {
        // Past the end pins to the end.
        assert_eq!(
            clamp_seek(Duration::from_secs(90), 60_000),
            Duration::from_secs(60)
        );
        // Within range passes through.
        assert_eq!(
            clamp_seek(Duration::from_secs(30), 60_000),
            Duration::from_secs(30)
        );
        // Unknown duration leaves the request untouched.
        assert_eq!(
            clamp_seek(Duration::from_secs(30), 0),
            Duration::from_secs(30)
        );
    }
}
