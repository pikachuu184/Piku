//! Audio playback backend for the preview panel.
//!
//! Holds a rodio output stream + sink. rodio does its own mixing on a
//! dedicated background thread, so appending a decoded source never blocks the
//! UI thread. The stream/sink are `!Send`, which is fine: gpui entities live
//! on the single UI thread (`Entity` only requires `'static`). While audio is
//! playing a lightweight ticker re-renders the panel so the transport bar
//! advances.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{Context, SharedString};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};

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
}

impl AudioPlayer {
    pub fn new() -> Self {
        Self { volume: 1.0, ..Self::default() }
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
        self.sink.as_ref().is_some_and(|s| !s.is_paused() && !s.empty())
    }

    pub fn position(&self) -> Duration {
        self.sink.as_ref().map(|s| s.get_pos()).unwrap_or(Duration::ZERO)
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
        self.meta = Some(meta.unwrap_or_else(|| TrackMeta {
            title: path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string().into(),
            ..TrackMeta::default()
        }));
        self.current = Some(path.clone());
        if let Some(old) = self.sink.take() {
            old.stop();
        }
        self.load_gen += 1;
        let generation = self.load_gen;
        let volume = self.effective_volume();

        let build = cx.background_executor().spawn(async move {
            // Reads go through the sanitizing local provider (refuses symlinks/dirs).
            let (file, total) = crate::storage::local().open_read(&path).ok()?;
            // Untrusted input: never stream an oversized file into the decoder.
            if total > MAX_PLAY_BYTES {
                return None;
            }
            // rodio/symphonia can *panic* (not just error) probing a malformed
            // or unsupported stream — catch it so a bad file is a no-op, not a
            // crash.
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let decoder = Decoder::new(BufReader::new(file)).ok()?;
                let sink = Sink::try_new(&handle).ok()?;
                sink.set_volume(volume);
                sink.append(decoder);
                Some(sink)
            }))
            .ok()
            .flatten()
        });
        cx.spawn(async move |this, cx| {
            let sink = build.await;
            let _ = this.update(cx, |this, cx| {
                // A newer load superseded this one — drop the stale sink.
                if this.load_gen != generation {
                    if let Some(sink) = sink {
                        sink.stop();
                    }
                    return;
                }
                if let Some(sink) = sink {
                    sink.play();
                    this.sink = Some(sink);
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
        cx.notify();
    }

    pub fn seek(&mut self, pos: Duration, cx: &mut Context<Self>) {
        // Clamp into the known track length so a scrubber drag past the end
        // can't hand the decoder an out-of-range position.
        let target = clamp_seek(pos, self.duration_ms());
        if let Some(sink) = &self.sink {
            let _ = sink.try_seek(target);
        }
        cx.notify();
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
        assert_eq!(clamp_seek(Duration::from_secs(90), 60_000), Duration::from_secs(60));
        // Within range passes through.
        assert_eq!(clamp_seek(Duration::from_secs(30), 60_000), Duration::from_secs(30));
        // Unknown duration leaves the request untouched.
        assert_eq!(clamp_seek(Duration::from_secs(30), 0), Duration::from_secs(30));
    }
}
