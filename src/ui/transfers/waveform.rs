//! The throughput waveform: what a transfer is *doing*, as opposed to how far
//! along it is.
//!
//! Completion and activity are two different signals and they get two different
//! widgets. The ring
//! ([`ProgressCircle`](gpui_component::progress::ProgressCircle) on the card)
//! answers "how much is left"; this answers "is anything moving right now". A
//! single progress bar cannot say both, which is why a copy that has stalled at
//! 60% for four minutes looks identical to one that is flying — the bar is at 60
//! either way.
//!
//! The bars themselves are painted by
//! [`components::paint_bars`](crate::ui::components::paint_bars), shared with the
//! audio scrubber, so the two rows of bars in the app cannot drift into looking
//! like different widgets. What is here is the box around them, the colour rule,
//! and the aggregate the popover draws.
//!
//! Samples arrive already normalized to `0.0..=1.0` from
//! [`transfer::rate`](crate::backend::services::transfer::rate), against a rolling
//! peak rather than an absolute rate — so the shape reads the same on a USB stick
//! as on an NVMe, and a stall is visible as bars settling toward the baseline
//! rather than as a row that was always short.

use gpui::{App, IntoElement, ParentElement as _, Styled as _, canvas, div, px};
use gpui_component::ActiveTheme as _;

use crate::services::jobs::Job;
use crate::ui::components::paint_bars;

/// Waveform height on a transfer-center card, in pixels.
pub const CARD_HEIGHT: f32 = 30.0;

/// Waveform height in the status-bar popover, where there is one row for the
/// whole subsystem and less room to give it.
pub const POPOVER_HEIGHT: f32 = 24.0;

/// One row of throughput bars, oldest sample on the left.
///
/// `live` picks the tone: `foreground` while bytes are moving, `muted_foreground`
/// otherwise. A paused or finished job keeps its last ring rather than blanking —
/// the shape is the record of what happened — and the dimmer tone is what says it
/// is history. Deciding this by colour rather than by clearing the samples means a
/// pause reads as "stopped here", which is what it is.
///
/// An empty `samples` draws the empty track alone. That is the honest rendering
/// for a job that has not been sampled yet (queued, or scanning) and for a simple
/// job that will never be sampled at all: a row of baseline bars would be a claim
/// that a reading exists.
pub fn waveform(samples: &[f32], height: f32, live: bool, cx: &App) -> impl IntoElement {
    // Owned, because the paint closure outlives this call.
    let values = samples.to_vec();
    let color = if live {
        cx.theme().foreground
    } else {
        cx.theme().muted_foreground
    };

    div()
        .w_full()
        .h(px(height))
        .flex_none()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .overflow_hidden()
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    // One tone for every bar: the played/unplayed split the
                    // scrubber needs is a statement about position, and position
                    // here is the ring's to make.
                    paint_bars(bounds, &values, |_| color, window);
                },
            )
            .size_full(),
        )
}

/// One ring standing in for every job that is moving right now.
///
/// The popover has room for a single row, and the question it answers is whether
/// the subsystem is busy — so this is the mean across live jobs, not the busiest
/// one. Two jobs each at their own peak still average to 1.0, because each ring is
/// already normalized against its own peak; what the mean actually protects
/// against is one idle job's shape being shown as if it were the whole picture.
///
/// Jobs are aligned on their *newest* sample rather than their oldest. Every ring
/// from [`rate::samples`](crate::backend::services::transfer::rate) is the same
/// length today, so this is currently a distinction without a difference — but a
/// shorter ring belongs to a job that started later, and left-aligning it would
/// put its most recent reading in the middle of the row.
pub fn aggregate(jobs: &[Job]) -> Vec<f32> {
    let rings: Vec<&[f32]> = jobs
        .iter()
        .filter(|job| job.is_moving() && !job.rate.is_empty())
        .map(|job| job.rate.as_slice())
        .collect();

    let width = rings.iter().map(|ring| ring.len()).max().unwrap_or(0);
    let mut out = Vec::with_capacity(width);
    for slot in 0..width {
        // Counted back from the newest sample, so slot `width - 1` is every
        // ring's last reading.
        let age = width - slot;
        let mut sum = 0.0;
        let mut contributors = 0.0;
        for ring in &rings {
            if let Some(value) = ring.len().checked_sub(age).and_then(|ix| ring.get(ix)) {
                sum += *value;
                contributors += 1.0;
            }
        }
        // Per-slot rather than one divisor for the row: a ring that does not reach
        // this far back must not drag the slots it never covered toward zero.
        out.push(if contributors > 0.0 {
            sum / contributors
        } else {
            0.0
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::jobs::{JobKind, JobStatus};

    fn moving(id: u64, rate: Vec<f32>) -> Job {
        let mut job = Job::new(id, JobKind::Copy, "Copying “a”".into());
        job.status = JobStatus::Running;
        job.rate = rate;
        job
    }

    #[test]
    fn one_moving_job_is_its_own_aggregate() {
        let jobs = vec![moving(1, vec![0.25, 0.5, 1.0])];
        assert_eq!(aggregate(&jobs), vec![0.25, 0.5, 1.0]);
    }

    #[test]
    fn two_jobs_average_slot_by_slot() {
        let jobs = vec![moving(1, vec![0.0, 1.0]), moving(2, vec![1.0, 0.0])];
        assert_eq!(aggregate(&jobs), vec![0.5, 0.5]);
    }

    /// A job that started later has a shorter ring, and its newest reading has to
    /// land in the newest slot — not the middle of the row.
    #[test]
    fn a_shorter_ring_lines_up_on_the_newest_sample() {
        let jobs = vec![moving(1, vec![0.2, 0.4, 0.6]), moving(2, vec![1.0])];
        // Only the last slot has two contributors, and it is the only one averaged.
        assert_eq!(aggregate(&jobs), vec![0.2, 0.4, 0.8]);
    }

    /// A paused job's ring is still drawn on its own card, dimmed — but it is not
    /// part of "is the subsystem busy", which is the only question the popover's
    /// single row answers.
    #[test]
    fn only_moving_jobs_are_counted() {
        let mut paused = moving(1, vec![1.0, 1.0]);
        paused.status = JobStatus::Paused;
        let mut done = moving(2, vec![1.0, 1.0]);
        done.status = JobStatus::Done;
        let queued = moving(3, Vec::new());

        assert!(aggregate(&[paused.clone(), done.clone(), queued.clone()]).is_empty());
        // …and a live job beside them is unaffected by their samples.
        assert_eq!(
            aggregate(&[paused, done, queued, moving(4, vec![0.5, 0.5])]),
            vec![0.5, 0.5]
        );
    }

    /// The popover renders this every 200 ms whether or not anything is running.
    #[test]
    fn nothing_running_aggregates_to_nothing_rather_than_to_zeroes() {
        assert!(aggregate(&[]).is_empty());
        assert!(aggregate(&[moving(1, Vec::new())]).is_empty());
    }
}
