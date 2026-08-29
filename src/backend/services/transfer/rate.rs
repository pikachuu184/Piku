//! Throughput sampling: the ring buffer behind the waveform, and the ETA.
//!
//! # Two signals, not one
//!
//! Completion and activity are different questions, and one bar cannot answer
//! both. A transfer stuck on a 4 GB file at 92% has a completion bar that looks
//! healthy and a throughput trace that is flat on the floor — which is the
//! information the user actually wants. So completion goes to the ring gauge
//! (`Progress`/`ProgressCircle`, fed by the protocol's own
//! [`Progress`](crate::backend::protocol::Progress)) and activity goes to the
//! waveform, fed by this.
//!
//! # Why the samples are normalized
//!
//! Absolute bytes per second would make a USB stick draw a permanently flat line
//! and an NVMe draw a permanently full one. Each ring normalizes against its own
//! recent history, so the *shape* — steady, stalling, recovering — reads the same
//! on both. The number in the numeric row stays absolute; that is where
//! "118 MB/s" belongs.
//!
//! # Why not against the peak
//!
//! Because the peak is usually a lie. The first small file of a copy is served
//! from page cache at 4 GB/s, the rest comes off the disk at 40 MB/s, and
//! dividing by the maximum draws the entire real transfer at 1% height. Decaying
//! the peak only trades one bug for a slower one: at a plausible per-tick decay a
//! 100× outlier still suppresses the display for forty seconds.
//!
//! So the scale is the 75th percentile of the rates currently on screen. A steady
//! transfer normalizes to full height, a genuine slowdown dips, and an outlier
//! pins at the ceiling instead of flattening everything else — which is also the
//! honest read: "that tick was the fastest this has been."

use std::collections::VecDeque;
use std::time::Duration;

/// How often the scheduler samples every live job.
///
/// 200 ms is five updates a second: fast enough that the waveform moves with the
/// transfer, slow enough that it is not the reason the transfer is slow.
pub const TICK: Duration = Duration::from_millis(200);

/// Ring slots, and therefore bars drawn.
///
/// 64 at 200 ms is a ~13 second window — long enough to show a stall developing,
/// short enough that the bars stay wide enough to see.
pub const SAMPLES: usize = 64;

/// Per-tick multiplier applied to an idle slot.
const DECAY: f32 = 0.85;

/// The height an idle waveform settles to.
///
/// Not zero: bars that snap flat read as "the widget broke", and a paused job is
/// not a broken one. This is the resting heartbeat.
const BASELINE: f32 = 0.08;

/// Ticks the ETA median is taken over.
const ETA_WINDOW: usize = 8;

/// Below this many samples the ETA is `—`.
///
/// Two samples of a copy that has not yet reached steady state produce a number
/// that is wrong by an order of magnitude, and a wrong ETA is worse than none.
const MIN_SAMPLES_FOR_ETA: usize = 3;

/// Percentile of the visible rates that draws at full height.
///
/// The top quartile clips, which is deliberate: clipping one outlier costs the
/// display nothing, and scaling to it costs the display everything.
const SCALE_PERCENTILE: usize = 75;

/// What one tick recorded.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Slot {
    /// Before the job produced this many samples.
    Empty,
    /// Bytes per second over the tick. Zero means the job was trying and got
    /// nowhere, which is a stall and is drawn as one.
    Moving(u64),
    /// The job was deliberately not moving: paused, queued, or waiting for a
    /// decision. Drawn as a decay from whatever precedes it, and excluded from
    /// the scale, because a paused job has no throughput to be a fraction of.
    Idle,
}

/// One job's throughput history.
#[derive(Debug)]
pub struct RateRing {
    slots: [Slot; SAMPLES],
    next: usize,
    /// Absolute bytes/sec, newest last, at most [`ETA_WINDOW`] long.
    recent: VecDeque<u64>,
    last: u64,
    ticks: usize,
}

impl Default for RateRing {
    fn default() -> Self {
        Self::new()
    }
}

impl RateRing {
    pub fn new() -> Self {
        Self {
            slots: [Slot::Empty; SAMPLES],
            next: 0,
            recent: VecDeque::with_capacity(ETA_WINDOW),
            last: 0,
            ticks: 0,
        }
    }

    /// Record one tick's worth of movement.
    ///
    /// `bytes` is the delta since the previous tick and `elapsed` the real time
    /// it took — measured rather than assumed, because a loaded machine delivers
    /// the 200 ms timer late and dividing by the nominal period would report a
    /// rate the disk never achieved.
    pub fn push(&mut self, bytes: u64, elapsed: Duration) {
        let secs = elapsed.as_secs_f32();
        let rate = if secs > 0.0 {
            (bytes as f32 / secs) as u64
        } else {
            0
        };

        self.last = rate;
        self.ticks = self.ticks.saturating_add(1);

        if self.recent.len() == ETA_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(rate);

        self.write(Slot::Moving(rate));
    }

    /// Record a tick during which the job was deliberately not moving —
    /// paused, queued behind a permit, or waiting for a decision.
    ///
    /// Distinct from `push(0, …)`: an idle tick is not a stall, so it must not
    /// enter the ETA window, where a run of zeros would drag the median to zero
    /// and turn a paused job's ETA into `—` forever after it resumes.
    pub fn idle(&mut self) {
        self.last = 0;
        self.write(Slot::Idle);
    }

    /// The slots oldest-first, which is left-to-right as the waveform draws.
    ///
    /// Resolving happens here rather than at write time because the scale is a
    /// property of the whole window: one tick can change what every earlier bar
    /// should be a fraction of.
    pub fn samples(&self) -> Vec<f32> {
        let scale = self.scale();
        let (tail, head) = self.slots.split_at(self.next);

        let mut out = Vec::with_capacity(SAMPLES);
        // One left-to-right sweep, so an idle run decays from the last real
        // reading and settles rather than snapping to the floor.
        let mut previous = BASELINE;
        for slot in head.iter().chain(tail.iter()) {
            let value = match *slot {
                Slot::Empty => BASELINE,
                Slot::Idle => (previous * DECAY).max(BASELINE),
                Slot::Moving(rate) => match scale {
                    Some(scale) => (rate as f32 / scale as f32).clamp(BASELINE, 1.0),
                    // Nothing on screen moved at all. Every bar is a stall, and
                    // a stall is drawn at rest, not at full height.
                    None => BASELINE,
                },
            };
            out.push(value);
            previous = value;
        }
        out
    }

    /// The most recent absolute rate, for the numeric row.
    pub fn bytes_per_sec(&self) -> u64 {
        self.last
    }

    /// Time remaining, or `None` while the estimate would be a guess.
    ///
    /// Median rather than mean: one stalled tick in eight moves a mean by an
    /// eighth and a median not at all, and an ETA that jumps every time a large
    /// file opens is an ETA nobody reads.
    pub fn eta(&self, remaining_bytes: u64) -> Option<Duration> {
        if self.recent.len() < MIN_SAMPLES_FOR_ETA {
            return None;
        }
        let rate = self.median();
        if rate == 0 {
            return None;
        }
        // Integer division at second granularity, so there is no float to
        // overflow `Duration::from_secs_f64` when the rate is a handful of bytes.
        Some(Duration::from_secs(remaining_bytes / rate))
    }

    /// How many samples have been taken. Exposed for the ETA gate's own test.
    pub fn ticks(&self) -> usize {
        self.ticks
    }

    fn write(&mut self, slot: Slot) {
        self.slots[self.next] = slot;
        self.next = (self.next + 1) % SAMPLES;
    }

    /// The rate that draws at full height, or `None` when nothing on screen
    /// moved.
    fn scale(&self) -> Option<u64> {
        let mut moving: Vec<u64> = self
            .slots
            .iter()
            .filter_map(|slot| match slot {
                Slot::Moving(rate) => Some(*rate),
                Slot::Empty | Slot::Idle => None,
            })
            .collect();
        if moving.is_empty() {
            return None;
        }
        moving.sort_unstable();
        let index = (moving.len() * SCALE_PERCENTILE / 100).min(moving.len() - 1);
        let scale = moving[index];
        (scale > 0).then_some(scale)
    }

    fn median(&self) -> u64 {
        let mut window: Vec<u64> = self.recent.iter().copied().collect();
        window.sort_unstable();
        let mid = window.len() / 2;
        if window.len().is_multiple_of(2) {
            // Mean of the two middles, averaged without overflowing.
            let (a, b) = (window[mid - 1], window[mid]);
            a + (b - a) / 2
        } else {
            window[mid]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn a_fresh_ring_rests_at_the_baseline() {
        let ring = RateRing::new();
        let samples = ring.samples();
        assert_eq!(samples.len(), SAMPLES);
        assert!(samples.iter().all(|&v| v == BASELINE));
        assert_eq!(ring.bytes_per_sec(), 0);
        assert_eq!(ring.eta(1_000), None, "no ETA before any samples");
    }

    #[test]
    fn a_steady_rate_normalizes_to_full_height() {
        let mut ring = RateRing::new();
        for _ in 0..10 {
            ring.push(1_000_000, SECOND);
        }
        assert_eq!(ring.bytes_per_sec(), 1_000_000);
        let samples = ring.samples();
        // The newest sample is last, and a rate that *is* the peak draws full.
        assert!(
            (samples[SAMPLES - 1] - 1.0).abs() < 0.01,
            "steady rate did not reach full height: {}",
            samples[SAMPLES - 1]
        );
    }

    /// Normalization is what makes the shape readable on any device: a USB stick
    /// and an NVMe running flat out both draw a full bar.
    #[test]
    fn the_shape_is_device_independent() {
        let mut slow = RateRing::new();
        let mut fast = RateRing::new();
        for _ in 0..8 {
            slow.push(2_000_000, SECOND); // 2 MB/s
            fast.push(2_000_000_000, SECOND); // 2 GB/s
        }
        let (s, f) = (slow.samples(), fast.samples());
        assert!(
            (s[SAMPLES - 1] - f[SAMPLES - 1]).abs() < 0.01,
            "same shape drew differently: {} vs {}",
            s[SAMPLES - 1],
            f[SAMPLES - 1]
        );
        // While the absolute numbers, which is what the numeric row shows, differ.
        assert_ne!(slow.bytes_per_sec(), fast.bytes_per_sec());
    }

    #[test]
    fn the_ring_wraps_and_keeps_the_newest_sample_last() {
        let mut ring = RateRing::new();
        // Two full laps plus one, so `next` has wrapped twice.
        for i in 0..(SAMPLES * 2 + 1) {
            ring.push(1_000 * (i as u64 + 1), SECOND);
        }
        let samples = ring.samples();
        assert_eq!(samples.len(), SAMPLES);
        // A monotonically rising rate ends at or above the scale, and the oldest
        // slot in the window must be lower than the newest.
        assert!(
            samples[SAMPLES - 1] > samples[0],
            "oldest-first ordering is wrong: {:?} .. {:?}",
            samples[0],
            samples[SAMPLES - 1]
        );
    }

    #[test]
    fn idle_ticks_decay_toward_the_baseline_and_stop_there() {
        let mut ring = RateRing::new();
        for _ in 0..4 {
            ring.push(1_000_000, SECOND);
        }
        let busy = ring.samples()[SAMPLES - 1];

        ring.idle();
        let first = ring.samples()[SAMPLES - 1];
        assert!(
            first < busy,
            "an idle tick did not decay: {busy} -> {first}"
        );
        assert!(first > BASELINE, "it fell straight to the floor");

        // Long enough to reach the floor, and it must not go through it.
        for _ in 0..100 {
            ring.idle();
        }
        let settled = ring.samples();
        assert!(
            settled.iter().all(|&v| (v - BASELINE).abs() < f32::EPSILON),
            "idle ring did not settle at the baseline: {:?}",
            &settled[..4]
        );
        assert_eq!(ring.bytes_per_sec(), 0);
    }

    /// A paused job's idle ticks must not poison the ETA. If they entered the
    /// median window, a job paused for two seconds would come back reporting no
    /// estimate at all.
    #[test]
    fn idle_ticks_stay_out_of_the_eta_window() {
        let mut ring = RateRing::new();
        for _ in 0..ETA_WINDOW {
            ring.push(1_000, SECOND);
        }
        let before = ring.eta(10_000).expect("an ETA after a steady run");

        for _ in 0..ETA_WINDOW {
            ring.idle();
        }
        assert_eq!(
            ring.eta(10_000),
            Some(before),
            "pausing changed the estimate"
        );
    }

    #[test]
    fn the_eta_waits_for_three_samples() {
        let mut ring = RateRing::new();
        ring.push(1_000, SECOND);
        assert_eq!(ring.eta(10_000), None);
        ring.push(1_000, SECOND);
        assert_eq!(ring.eta(10_000), None, "two samples is still a guess");
        ring.push(1_000, SECOND);
        assert_eq!(
            ring.eta(10_000),
            Some(Duration::from_secs(10)),
            "10 000 bytes at 1 000 B/s"
        );
        assert_eq!(ring.ticks(), MIN_SAMPLES_FOR_ETA);
    }

    /// The reason for a median: one stall must not swing the clock.
    #[test]
    fn one_stalled_tick_does_not_move_the_estimate() {
        let mut steady = RateRing::new();
        let mut stalled = RateRing::new();
        for _ in 0..ETA_WINDOW {
            steady.push(1_000, SECOND);
            stalled.push(1_000, SECOND);
        }
        // Replace the newest sample in one ring with a stall.
        stalled.push(0, SECOND);

        let (a, b) = (steady.eta(100_000), stalled.eta(100_000));
        assert_eq!(a, Some(Duration::from_secs(100)));
        assert_eq!(b, a, "a single zero sample moved the ETA: {a:?} vs {b:?}");
    }

    #[test]
    fn a_fully_stalled_job_reports_no_estimate() {
        let mut ring = RateRing::new();
        for _ in 0..ETA_WINDOW {
            ring.push(0, SECOND);
        }
        assert_eq!(ring.eta(10_000), None);
        // But it is drawn as stalled, not as absent.
        assert!(ring.samples().iter().all(|&v| v >= BASELINE));
    }

    /// A burst from page cache must not flatten every later bar. This is the bug
    /// where a copy looks stopped after its first small file, and the reason the
    /// scale is a percentile rather than the maximum.
    #[test]
    fn a_one_off_burst_does_not_flatten_the_real_rate() {
        let mut ring = RateRing::new();
        ring.push(4_000_000_000, SECOND); // cached read
        for _ in 0..60 {
            ring.push(40_000_000, SECOND); // the real disk
        }
        let newest = ring.samples()[SAMPLES - 1];
        assert!(
            newest > 0.5,
            "the burst is still suppressing the real rate: {newest}"
        );
    }

    /// The burst itself is not hidden either — it pins at the ceiling rather
    /// than being scaled away, because "that was the fastest this has been" is
    /// the true reading.
    #[test]
    fn the_burst_itself_still_draws_at_full_height() {
        let mut ring = RateRing::new();
        ring.push(4_000_000_000, SECOND);
        for _ in 0..10 {
            ring.push(40_000_000, SECOND);
        }
        let samples = ring.samples();
        // 11 filled slots, so the burst sits at index SAMPLES - 11.
        let burst = samples[SAMPLES - 11];
        assert!((burst - 1.0).abs() < f32::EPSILON, "burst drew at {burst}");
    }

    /// A real halving of throughput has to be visible. If the scale absorbed it,
    /// the waveform would be decorative.
    #[test]
    fn a_genuine_slowdown_is_visible() {
        let mut ring = RateRing::new();
        for _ in 0..32 {
            ring.push(100_000_000, SECOND);
        }
        for _ in 0..32 {
            ring.push(25_000_000, SECOND);
        }
        let samples = ring.samples();
        assert!(
            (samples[0] - 1.0).abs() < 0.01,
            "the fast half did not draw full: {}",
            samples[0]
        );
        assert!(
            samples[SAMPLES - 1] < 0.35,
            "a 4x slowdown drew at {}",
            samples[SAMPLES - 1]
        );
    }

    /// The failure mode of normalizing against a decaying peak: as the peak
    /// falls, the decayed tail gets re-scaled back up, so a job paused for a
    /// minute slowly creeps back to full height. Idle slots derive from the slot
    /// to their left instead, so they cannot.
    #[test]
    fn a_paused_tail_does_not_creep_back_up() {
        let mut ring = RateRing::new();
        for _ in 0..8 {
            ring.push(100_000_000, SECOND);
        }
        for _ in 0..12 {
            ring.idle();
        }
        let early = ring.samples()[SAMPLES - 1];

        for _ in 0..200 {
            ring.idle();
        }
        let late = ring.samples()[SAMPLES - 1];
        assert!(
            late <= early,
            "the idle tail rose from {early} to {late} while nothing moved"
        );
        assert!((late - BASELINE).abs() < f32::EPSILON, "settled at {late}");
    }

    /// The rate is measured against real elapsed time, not the nominal tick, so
    /// a late timer does not invent throughput.
    #[test]
    fn a_late_tick_does_not_inflate_the_rate() {
        let mut on_time = RateRing::new();
        let mut late = RateRing::new();
        on_time.push(200_000, TICK);
        late.push(200_000, TICK * 4);
        assert_eq!(on_time.bytes_per_sec(), 1_000_000);
        assert_eq!(late.bytes_per_sec(), 250_000);
    }

    #[test]
    fn a_zero_length_tick_is_not_a_division_by_zero() {
        let mut ring = RateRing::new();
        ring.push(1_000, Duration::ZERO);
        assert_eq!(ring.bytes_per_sec(), 0);
        assert!(ring.samples().iter().all(|v| v.is_finite()));
    }

    #[test]
    fn samples_never_leave_the_drawable_range() {
        let mut ring = RateRing::new();
        for i in 0..200u64 {
            // Wildly varying, including zeros and one enormous spike.
            let bytes = if i % 7 == 0 { 0 } else { i * i * 1_000_000 };
            ring.push(bytes, TICK);
            assert!(
                ring.samples()
                    .iter()
                    .all(|&v| (0.0..=1.0).contains(&v) && v.is_finite()),
                "a sample escaped 0.0..=1.0 at tick {i}"
            );
        }
    }
}
