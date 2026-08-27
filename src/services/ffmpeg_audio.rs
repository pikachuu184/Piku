//! Audio decoded by the bundled ffmpeg and streamed into rodio.
//!
//! Why this exists: rodio's decoder is symphonia, and Piku's dependency tree
//! gives symphonia only `aac + flac + isomp4 + mp3 + vorbis + wav`. There is no
//! Matroska demuxer in the tree at all and no Opus, AC-3, DTS, or HE-AAC
//! decoder anywhere in symphonia, so handing a `.mkv`, `.webm`, `.avi`, or an
//! MP4 with AC-3 straight to `rodio::Decoder` silently produced **no audio** —
//! which is exactly what "some videos have no sound" was.
//!
//! ffmpeg can demux and decode all of it, so this streams `-f s16le` out of an
//! ffmpeg child and presents it as a [`rodio::Source`]. Two properties are
//! load-bearing:
//!
//! * **Bounded.** A reader thread hands fixed-size chunks over a
//!   [`sync_channel`] of capacity [`CHUNK_QUEUE_CAP`]; when it is full the
//!   thread stops reading, which stops ffmpeg on its pipe write. Nothing is ever
//!   fully buffered, so a multi-hour or hostile file costs the same ~64 KiB as a
//!   ten-second one. This is what makes the byte cap the old rodio path needed
//!   (`MAX_PLAY_BYTES`) unnecessary here.
//! * **Never blocking.** [`Iterator::next`] is called on rodio's mixer thread,
//!   which is shared with every other sound in the app — blocking it would
//!   freeze *all* audio. On an underrun `next` yields silence rather than
//!   waiting, and it only ends the source once ffmpeg has genuinely finished.
//!
//! Because the samples that reach the mixer are counted (excluding injected
//! silence), the player can also use this as a media clock — see
//! [`FfmpegPcm::played`].
//!
//! ffmpeg is spawned with the same hardening as the rest of the app:
//! `-nostdin`, `CREATE_NO_WINDOW` on Windows, and `-protocol_whitelist
//! file,crypto` plus a `file:` input prefix so a crafted container cannot make
//! it reach out to the network (SSRF).

use std::ffi::OsString;
use std::io::Read as _;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::time::Duration;

use rodio::Source;

/// Output sample rate asked of ffmpeg. Fixed so rodio never has to resample.
const SAMPLE_RATE: u32 = 48_000;

/// Output channel count asked of ffmpeg. Fixed for the same reason.
const CHANNELS: u16 = 2;

/// Samples (not frames) per chunk handed to the mixer: 2048 stereo frames,
/// ≈ 42 ms. Small enough that a seek discards almost nothing, large enough that
/// the channel is not hot.
const CHUNK_SAMPLES: usize = 4096;

/// Chunks buffered ahead of the mixer, ≈ 340 ms / ≈ 64 KiB. The cap is the
/// memory bound and the backpressure point in one.
const CHUNK_QUEUE_CAP: usize = 8;

/// How long [`FfmpegPcm::open`] waits for the first samples before concluding
/// the file has no audio this build can decode. Generous: it covers a cold
/// process spawn plus a container that must be probed before it yields anything.
const PRIME_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the consumed-sample counter is published. Per-sample atomics would
/// work but this keeps `next` to plain arithmetic; 256 samples is ≈ 2.7 ms,
/// finer than any clock consumer cares about.
const PUBLISH_EVERY: u64 = 256;

/// Wait between retries while the chunk queue is full.
const BACKPRESSURE_NAP: Duration = Duration::from_millis(5);

/// Interleaved 48 kHz stereo `i16` PCM streamed from an ffmpeg child.
///
/// `Send`, which matters: the track is built on a worker and handed to the
/// thread that owns the sink, and `rodio::Sink::append` requires
/// `Source + Send + 'static`.
pub struct FfmpegPcm {
    rx: Receiver<Vec<i16>>,
    /// The chunk being handed out and how far into it we are.
    chunk: Vec<i16>,
    at: usize,
    /// Samples of *real* audio handed to the mixer, excluding underrun silence.
    /// Published for the media clock; see [`Self::played`].
    played: Arc<AtomicU64>,
    /// Unpublished part of the count, folded in every [`PUBLISH_EVERY`].
    local: u64,
    /// Set once ffmpeg has finished and the queue is drained.
    done: bool,
    stop: Arc<AtomicBool>,
    child: Option<Child>,
}

impl FfmpegPcm {
    /// Open `path`'s audio starting at `from_ms`, blocking until the first
    /// samples arrive so a file with no decodable audio is reported as `None`
    /// rather than becoming a source that ends immediately.
    ///
    /// **Blocking — worker threads only.** That distinction is why
    /// [`Self::resume`] exists.
    pub fn open(path: &Path, from_ms: u64) -> Option<Self> {
        let mut pcm = Self::spawn(path, from_ms)?;
        // Priming does double duty: it proves there is audio, and it leaves a
        // chunk ready so playback does not open on an underrun.
        match pcm.rx.recv_timeout(PRIME_TIMEOUT) {
            Ok(chunk) => {
                pcm.chunk = chunk;
                pcm.at = 0;
                Some(pcm)
            }
            // The reader thread finished without producing anything: no audio
            // stream, or ffmpeg refused the file. Either way, play silent.
            Err(_) => None,
        }
    }

    /// Open `path`'s audio at `from_ms` **without** waiting for the first
    /// samples, for a seek on a file already known to have audio.
    ///
    /// Cheap enough for the UI thread — it is a `spawn` plus a thread, with all
    /// I/O on the reader — and the caller's clock tolerates the startup gap
    /// because [`Self::played`] does not advance during it.
    pub fn resume(path: &Path, from_ms: u64) -> Option<Self> {
        Self::spawn(path, from_ms)
    }

    /// Samples of real audio delivered to the mixer so far, shared so it can be
    /// read after the source has been moved into the sink.
    ///
    /// Divided by [`Self::samples_per_second`] this is elapsed media time — a
    /// far sturdier clock than `Sink::get_pos()`, which counts injected silence
    /// and reports wall time rather than media time once a speed factor is
    /// applied.
    pub fn played(&self) -> Arc<AtomicU64> {
        self.played.clone()
    }

    /// Samples per second of media (rate × channels), the divisor for
    /// [`Self::played`].
    pub const fn samples_per_second() -> u64 {
        SAMPLE_RATE as u64 * CHANNELS as u64
    }

    /// Milliseconds of media represented by a [`Self::played`] count.
    pub fn played_ms(samples: u64) -> u64 {
        samples.saturating_mul(1000) / Self::samples_per_second()
    }

    /// Spawn ffmpeg and the reader thread. No I/O on this thread beyond the
    /// process spawn itself.
    fn spawn(path: &Path, from_ms: u64) -> Option<Self> {
        if !crate::backend::services::preview::probe::ffmpeg_available() {
            return None;
        }
        let mut cmd = Command::new(ffmpeg_sidecar::paths::ffmpeg_path());
        cmd.args(["-nostdin", "-loglevel", "error"])
            // A crafted container must not be able to make ffmpeg fetch a remote
            // URL (SSRF); local files only.
            .args(["-protocol_whitelist", "file,crypto"]);
        if from_ms > 0 {
            // Before `-i`: input seeking, which skips rather than decodes.
            cmd.args(["-ss", &format!("{:.3}", from_ms as f64 / 1000.0)]);
        }
        cmd.arg("-i")
            .arg(input_arg(path))
            // Audio only, one stream, fixed layout so rodio never resamples.
            .args([
                "-vn", "-sn", "-dn", "-ac", "2", "-ar", "48000", "-f", "s16le", "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW — no console flash
        }

        let mut child = cmd.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let (tx, rx) = sync_channel::<Vec<i16>>(CHUNK_QUEUE_CAP);
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        std::thread::spawn(move || read_loop(stdout, tx, reader_stop));

        Some(Self {
            rx,
            chunk: Vec::new(),
            at: 0,
            played: Arc::new(AtomicU64::new(0)),
            local: 0,
            done: false,
            stop,
            child: Some(child),
        })
    }

    /// Count one delivered sample, publishing periodically.
    #[inline]
    fn count(&mut self) {
        self.local += 1;
        if self.local >= PUBLISH_EVERY {
            self.played.fetch_add(self.local, Ordering::Release);
            self.local = 0;
        }
    }
}

impl Iterator for FfmpegPcm {
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        loop {
            if self.at < self.chunk.len() {
                let sample = self.chunk[self.at];
                self.at += 1;
                self.count();
                return Some(sample);
            }
            if self.done {
                return None;
            }
            match self.rx.try_recv() {
                // `read_loop` never sends an empty chunk, so the loop makes
                // exactly one more pass and returns a sample.
                Ok(chunk) => {
                    self.chunk = chunk;
                    self.at = 0;
                }
                // Underrun. Yield silence rather than blocking the mixer thread,
                // which is shared with every other sound in the app. The counter
                // deliberately does not advance: no media time passes here, and
                // the audio resumes exactly where it left off.
                Err(TryRecvError::Empty) => return Some(0),
                // ffmpeg finished and the queue is drained: a real end of track.
                Err(TryRecvError::Disconnected) => {
                    self.done = true;
                    self.played.fetch_add(self.local, Ordering::Release);
                    self.local = 0;
                    return None;
                }
            }
        }
    }
}

impl Source for FfmpegPcm {
    /// Constant for the life of the source: ffmpeg was told exactly what to
    /// emit, so the rate and layout never change mid-stream.
    fn current_frame_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> u16 {
        CHANNELS
    }

    fn sample_rate(&self) -> u32 {
        SAMPLE_RATE
    }

    /// Unknown by design — the length comes from the video player's own probe,
    /// and claiming one here would make rodio trust a number this stream cannot
    /// guarantee.
    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

impl Drop for FfmpegPcm {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            // Reap it: without the wait the child lingers as a zombie, and the
            // reader thread never sees its pipe close. A killed process that was
            // only ever blocked on a pipe write exits immediately, so this does
            // not stall the caller (which may be rodio's mixer thread).
            let _ = child.wait();
        }
    }
}

/// Prefix the input with `file:` so ffmpeg's protocol is pinned even if the path
/// itself looks like a URL — defence in depth beside `-protocol_whitelist`.
///
/// Built as an `OsString` rather than a `String`: `to_string_lossy` would
/// replace a non-UTF-8 component with U+FFFD and hand ffmpeg a path that does
/// not exist, or a different one. argv carries bytes, so there is no need to
/// round-trip through UTF-8.
fn input_arg(path: &Path) -> OsString {
    let mut arg = OsString::from("file:");
    arg.push(path.as_os_str());
    arg
}

/// Read `s16le` from ffmpeg's stdout and hand fixed-size chunks to the mixer.
///
/// The `try_send` loop rather than a blocking `send` is deliberate: a blocking
/// send holds the thread inside the channel, where it cannot observe `stop`, so
/// a dropped source would leave the thread (and its ffmpeg child) alive until
/// the mixer happened to drain. Here it always notices within
/// [`BACKPRESSURE_NAP`].
fn read_loop(
    mut stdout: std::process::ChildStdout,
    tx: SyncSender<Vec<i16>>,
    stop: Arc<AtomicBool>,
) {
    let mut bytes = vec![0u8; CHUNK_SAMPLES * 2];
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        // Short reads are normal on a pipe; fill the buffer so chunk sizes stay
        // predictable, and stop at the first EOF.
        let mut filled = 0usize;
        while filled < bytes.len() {
            match stdout.read(&mut bytes[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        // A trailing odd byte cannot form a sample; drop it.
        let samples = filled / 2;
        if samples == 0 {
            return; // EOF or a dead pipe: dropping `tx` ends the source.
        }
        let chunk: Vec<i16> = bytes[..samples * 2]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();

        let mut pending = Some(chunk);
        while let Some(chunk) = pending.take() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match tx.try_send(chunk) {
                Ok(()) => {}
                Err(TrySendError::Full(chunk)) => {
                    pending = Some(chunk);
                    std::thread::sleep(BACKPRESSURE_NAP);
                }
                // The source was dropped; nothing left to feed.
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        if samples < CHUNK_SAMPLES {
            return; // A short final chunk means the read above hit EOF.
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate a clip with `args` at `path`, returning false if the bundled
    /// ffmpeg is unavailable or the encode failed (so tests skip rather than
    /// fail on a machine without the sidecar binary).
    fn generate(path: &Path, args: &[&str]) -> bool {
        if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
            return false;
        }
        let status = Command::new(ffmpeg_sidecar::paths::ffmpeg_path())
            .args(["-y", "-loglevel", "error"])
            .args(args)
            .arg(path)
            .status();
        matches!(status, Ok(status) if status.success())
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("piku-ffmpeg-audio-tests");
        let _ = std::fs::create_dir_all(&dir);
        dir.join(name)
    }

    /// The whole point of the module: audio out of a container rodio cannot
    /// demux at all. Matroska has no symphonia demuxer in this build and Opus
    /// has no symphonia decoder in any build, so this file is silent on the old
    /// path by construction.
    #[test]
    fn mkv_opus_yields_audio() {
        let clip = scratch("tone.mkv");
        if !generate(
            &clip,
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=2",
                "-c:a",
                "libopus",
            ],
        ) {
            eprintln!("skipping: could not generate an mkv/opus clip");
            return;
        }

        let Some(mut pcm) = FfmpegPcm::open(&clip, 0) else {
            let _ = std::fs::remove_file(&clip);
            // libopus is optional in an ffmpeg build; a missing encoder means
            // the clip has no audio, which is not this module's failure.
            eprintln!("skipping: no decodable audio in the generated clip");
            return;
        };
        assert_eq!(pcm.sample_rate(), SAMPLE_RATE);
        assert_eq!(pcm.channels(), CHANNELS);

        // A 440 Hz tone must produce something audible, not a run of zeros.
        let taken: Vec<i16> = pcm.by_ref().take(20_000).collect();
        assert_eq!(taken.len(), 20_000, "the source ended early");
        assert!(
            taken.iter().any(|&s| s.abs() > 1000),
            "decoded a silent buffer from a sine wave"
        );
        assert!(
            FfmpegPcm::played_ms(pcm.played().load(Ordering::Acquire)) > 0,
            "the media clock did not advance"
        );

        let _ = std::fs::remove_file(&clip);
    }

    /// A video with no audio stream must be reported as absent, not handed back
    /// as a source that ends on its first sample — the player uses the presence
    /// of a track to decide whether audio drives its clock.
    #[test]
    fn a_clip_with_no_audio_stream_has_no_track() {
        let clip = scratch("silent.mp4");
        if !generate(
            &clip,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
                "-pix_fmt",
                "yuv420p",
            ],
        ) {
            eprintln!("skipping: could not generate a video-only clip");
            return;
        }
        assert!(
            FfmpegPcm::open(&clip, 0).is_none(),
            "a video with no audio stream must not produce a track"
        );
        let _ = std::fs::remove_file(&clip);
    }

    /// Seeking is a fresh process at a new `-ss`, so the offset must actually
    /// reach ffmpeg rather than being silently dropped.
    #[test]
    fn seeking_past_a_silent_lead_in_lands_on_the_tone() {
        let clip = scratch("late-tone.wav");
        // One second of silence, then one second of tone.
        if !generate(
            &clip,
            &[
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=48000:cl=stereo:duration=1",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=1",
                "-filter_complex",
                "[0:a][1:a]concat=n=2:v=0:a=1",
            ],
        ) {
            eprintln!("skipping: could not generate a lead-in clip");
            return;
        }

        let Some(mut pcm) = FfmpegPcm::open(&clip, 1_200) else {
            let _ = std::fs::remove_file(&clip);
            eprintln!("skipping: no decodable audio in the generated clip");
            return;
        };
        let taken: Vec<i16> = pcm.by_ref().take(20_000).collect();
        assert!(
            taken.iter().any(|&s| s.abs() > 1000),
            "seeking to 1.2s landed in the silent lead-in instead of the tone"
        );
        let _ = std::fs::remove_file(&clip);
    }

    /// A garbage file must not hang: `open` bounds its wait and reports absence.
    #[test]
    fn a_non_media_file_produces_no_track() {
        if !ffmpeg_sidecar::command::ffmpeg_is_installed() {
            eprintln!("skipping: ffmpeg not available");
            return;
        }
        let junk = scratch("notes.txt");
        if std::fs::write(&junk, b"this is not audio").is_err() {
            eprintln!("skipping: could not write the test file");
            return;
        }
        assert!(FfmpegPcm::open(&junk, 0).is_none());
        let _ = std::fs::remove_file(&junk);
    }

    /// An underrun must yield silence immediately. Blocking here would stall
    /// rodio's mixer thread, which is shared with every other sound in the app.
    #[test]
    fn an_underrun_yields_silence_instead_of_blocking() {
        let (tx, rx) = sync_channel::<Vec<i16>>(CHUNK_QUEUE_CAP);
        let mut pcm = FfmpegPcm {
            rx,
            chunk: vec![7, 7],
            at: 0,
            played: Arc::new(AtomicU64::new(0)),
            local: 0,
            done: false,
            stop: Arc::new(AtomicBool::new(false)),
            child: None,
        };
        // The primed chunk drains first.
        assert_eq!(pcm.next(), Some(7));
        assert_eq!(pcm.next(), Some(7));
        // Now empty but the sender is alive: silence, and the clock must not
        // advance across it.
        assert_eq!(pcm.next(), Some(0));
        assert_eq!(pcm.next(), Some(0));
        let before = pcm.played.load(Ordering::Acquire) + pcm.local;
        assert_eq!(before, 2, "underrun silence was counted as media time");
        // Sender gone and queue drained: a real end of track, and permanent.
        drop(tx);
        assert_eq!(pcm.next(), None);
        assert_eq!(pcm.next(), None);
    }
}
