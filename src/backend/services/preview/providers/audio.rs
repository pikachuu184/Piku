//! Audio preview: tags and properties via `lofty`, plus a peak waveform for
//! the transport scrubber.
//!
//! The waveform is the expensive part — it decodes the whole file — so it is
//! capped by size, bounded in memory by a self-coarsening envelope, and
//! interruptible. rodio/symphonia can *panic* on a malformed stream, so the
//! decode runs inside `catch_unwind`; a panic degrades to an empty waveform
//! (and then the ffmpeg fallback), never an abort.

use std::io::BufReader;
use std::path::Path;

use crate::backend::error::PreviewError;
use crate::backend::protocol::Cancel;
use crate::backend::services::preview::content::{MetaRow, PreviewPayload};
use crate::backend::services::preview::{
    AUDIO_WAVEFORM_MAX_BYTES, LoadCtx, PreviewKind, PreviewProvider, WAVEFORM_BUCKETS, probe, read,
};

/// Samples between cancellation checks. Frequent enough that a superseded
/// selection stops within a frame, rare enough not to show up in the profile.
const CANCEL_EVERY: u64 = 65_536;

pub struct AudioMeta;

impl PreviewProvider for AudioMeta {
    fn kind(&self) -> PreviewKind {
        PreviewKind::AudioMeta
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        use lofty::prelude::*;

        ctx.cancel.check()?;
        let (file, total) = read::open_read(ctx.path)?;

        let undecodable = |error: &dyn std::fmt::Display| {
            PreviewError::Undecodable(
                format!("could not read audio metadata: {error}")
                    .as_str()
                    .into(),
            )
        };
        let probe_handle = lofty::probe::Probe::new(BufReader::new(file))
            .guess_file_type()
            .map_err(|e| undecodable(&e))?;
        let tagged = probe_handle.read().map_err(|e| undecodable(&e))?;

        let mut rows: Vec<MetaRow> = Vec::new();
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            let mut push = |label: &str, value: Option<std::borrow::Cow<'_, str>>| {
                if let Some(value) = value
                    && !value.is_empty()
                {
                    // Tag values are arbitrary text from a file the user
                    // downloaded, and they render in the inspector, the media
                    // panel, and the transport bar.
                    let clean = crate::security::text::sanitize_label(&value);
                    if !clean.is_empty() {
                        rows.push((label.into(), clean.as_str().into()));
                    }
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
        rows.push((
            "Duration".into(),
            format!("{}:{:02}", seconds / 60, seconds % 60)
                .as_str()
                .into(),
        ));
        if let Some(bitrate) = properties.overall_bitrate() {
            rows.push(("Bitrate".into(), format!("{bitrate} kbps").as_str().into()));
        }
        if let Some(rate) = properties.sample_rate() {
            rows.push(("Sample rate".into(), format!("{rate} Hz").as_str().into()));
        }
        if let Some(channels) = properties.channels() {
            rows.push(("Channels".into(), channels.to_string().as_str().into()));
        }

        // Untrusted input: only re-decode the whole file for peaks when it is
        // within the size cap; otherwise the scrubber renders without a
        // waveform.
        let waveform = if total <= AUDIO_WAVEFORM_MAX_BYTES {
            let mut peaks = audio_waveform(ctx.path, ctx.cancel);
            // rodio/symphonia can't decode every format lofty can still tag
            // (some WMA/AAC). When it yields nothing, fall back to the bundled
            // ffmpeg, which decodes far more — so the waveform still displays.
            if peaks.iter().all(|p| *p <= 0.0)
                && let Some(fallback) =
                    probe::audio_pcm_peaks(ctx.path, WAVEFORM_BUCKETS, ctx.cancel)
            {
                peaks = fallback;
            }
            peaks
        } else {
            Vec::new()
        };
        // A waveform that stopped early is not a waveform; report the
        // cancellation rather than caching a half-drawn scrubber.
        ctx.cancel.check()?;

        Ok(PreviewPayload::Audio {
            rows,
            waveform,
            duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
        })
    }
}

/// Coarse peak waveform (0..1, normalized). Failure → empty (the caller then
/// tries the ffmpeg fallback).
fn audio_waveform(path: &Path, cancel: &Cancel) -> Vec<f32> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        audio_waveform_decode(path, cancel)
    }))
    .unwrap_or_default()
}

/// Decodes the whole file, building a max-amplitude envelope that self-coarsens
/// as it grows — so it needs **no** up-front length estimate and a
/// missing/zero duration can't collapse every sample into one bucket (the
/// previous bug). Capped so a pathological input can't spin forever.
fn audio_waveform_decode(path: &Path, cancel: &Cancel) -> Vec<f32> {
    use rodio::Source as _;

    const MAX_SAMPLES: u64 = 60_000_000;

    let Ok((file, _)) = read::open_read(path) else {
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
        // Decoding a two-hour file is the single longest thing a preview does.
        // Without this, arrowing past an album keeps every one of them running.
        if idx.is_multiple_of(CANCEL_EVERY) && cancel.is_cancelled() {
            return Vec::new();
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            halve_envelope(&[0.1, 0.9, 0.5, 0.2, 0.7]),
            vec![0.9, 0.5, 0.7]
        );
    }

    #[test]
    fn resample_empty_is_empty() {
        assert!(resample_peaks(&[], 240).is_empty());
    }
}
