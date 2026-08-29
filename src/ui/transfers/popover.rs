//! The compact transfer surface: one line of what the subsystem is doing, one
//! row of bars saying whether it is moving, and the two controls that apply to
//! everything at once.
//!
//! Anchored above the status bar's job segment, which is on the right — so this
//! is the same overlay block `shell.rs` uses for the branch panel, mirrored. The
//! chrome is copied from [`git::BranchPopover`](crate::ui::git::BranchPopover)
//! deliberately: a second floating panel that looked like a different kind of
//! object would be a second idiom for the same thing.
//!
//! # What is in here and what is not
//!
//! Per-job detail is the transfer center's, not this panel's. `Details` opens it.
//! The one exception is a job stopped for a decision: waiting is the state where
//! nothing happens until a human acts, so it gets a row and a button rather than
//! sitting two clicks away behind `Details`.
//!
//! Throughput is summed across moving jobs and the waveform is their mean — see
//! [`waveform::aggregate`](crate::ui::transfers::waveform::aggregate) for why a
//! mean rather than the busiest one. Both read only jobs that are actually
//! moving: a paused job's last reading is history, and adding it to a total would
//! report bytes per second that nothing is producing.

use gpui::{
    Context, IntoElement, ParentElement as _, Render, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::services::jobs::{Job, JobStatus, label};
use crate::state::PikuState;
use crate::ui::transfers::waveform::{POPOVER_HEIGHT, aggregate, waveform};

/// Bytes per second across every job that is moving right now.
///
/// Only moving jobs: a paused job's `throughput` is the last reading before it
/// stopped, and adding it to a total would claim a rate nothing is producing.
/// Saturating, because a total is not worth a panic in a render pass.
fn total_throughput(jobs: &[Job]) -> u64 {
    jobs.iter()
        .filter(|job| job.is_moving())
        .fold(0u64, |sum, job| sum.saturating_add(job.throughput))
}

/// The first job stopped for a decision, if any — the one `Resolve` answers.
///
/// First rather than a count of them: the engine asks one job at a time per ask,
/// and answering the oldest is the order they stopped in.
fn first_waiting(jobs: &[Job]) -> Option<(u64, String)> {
    jobs.iter()
        .find(|job| job.status == JobStatus::WaitingForInput)
        .map(|job| (job.id, job.title.to_string()))
}

pub struct TransferPopover;

impl TransferPopover {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let jobs = PikuState::global(cx).jobs.clone();
        // The tick fires five times a second while anything is running; this is
        // what turns it into a redraw of the bars.
        cx.observe(&jobs, |_, _, cx| cx.notify()).detach();
        Self
    }
}

impl Render for TransferPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("TransferPopover");
        let store = PikuState::global(cx).jobs.clone();
        let (headline, bars, rate, waiting, can_pause, can_resume) = {
            let jobs = store.read(cx);
            let all = jobs.jobs();
            (
                label::activity_summary(all),
                aggregate(all),
                total_throughput(all),
                first_waiting(all),
                jobs.can_pause_any(),
                jobs.can_resume_any(),
            )
        };
        let live = rate > 0;

        let mut panel = v_flex()
            .w(px(300.))
            .p_2()
            .gap_2()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .shadow_none()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(cx.theme().foreground)
                            .child(headline),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(cx.theme().muted_foreground)
                            .child(label::throughput(rate)),
                    ),
            );

        // No samples means nothing has been measured — a queued job, or a
        // subsystem at rest. A row of baseline bars would be a claim that a
        // reading exists.
        if !bars.is_empty() {
            panel = panel.child(waveform(&bars, POPOVER_HEIGHT, live, cx));
        }

        if let Some((id, title)) = waiting {
            panel = panel.child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            // Grayscale emphasis, as everywhere else on this
                            // surface: the thing that needs a human gets the
                            // brightest tier, not a colour.
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    .child(
                        Button::new("transfers-resolve")
                            .label(label::RESOLVE)
                            .xsmall()
                            .ghost()
                            .tooltip("Answer this transfer's conflicts")
                            .on_click(move |_, window, cx| {
                                crate::ui::transfers::conflicts::ConflictView::open(id, window, cx);
                            }),
                    ),
            );
        }

        panel.child(
            h_flex()
                .w_full()
                .gap_1()
                .items_center()
                .when(can_pause, |row| {
                    row.child(
                        Button::new("transfers-pause-all")
                            .label(label::PAUSE_ALL)
                            .xsmall()
                            .ghost()
                            .on_click({
                                let store = store.clone();
                                move |_, _, cx| {
                                    store.update(cx, |jobs, cx| jobs.pause_all(cx));
                                }
                            }),
                    )
                })
                .when(can_resume, |row| {
                    row.child(
                        Button::new("transfers-resume-all")
                            .label(label::RESUME_ALL)
                            .xsmall()
                            .ghost()
                            .on_click({
                                let store = store.clone();
                                move |_, _, cx| {
                                    store.update(cx, |jobs, cx| jobs.resume_all(cx));
                                }
                            }),
                    )
                })
                .child(div().flex_1())
                .child(
                    Button::new("transfers-details")
                        .label(label::DETAILS)
                        .xsmall()
                        .ghost()
                        .tooltip("Open the transfer center (Ctrl+Shift+T)")
                        // Dispatched rather than opened here, so the shell owns
                        // the one flag that says whether a center is on the
                        // dialog stack — and closes this panel, which the
                        // dialog's overlay would otherwise leave visible and
                        // unclickable.
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                Box::new(crate::app::actions::ToggleTransferCenter),
                                cx,
                            );
                        }),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::jobs::JobKind;

    fn job(id: u64, status: JobStatus, throughput: u64) -> Job {
        let mut job = Job::new(id, JobKind::Copy, "Copying “a”".into());
        job.status = status;
        job.throughput = throughput;
        job
    }

    #[test]
    fn only_moving_jobs_contribute_to_the_total() {
        let jobs = vec![
            job(1, JobStatus::Running, 1_000),
            job(2, JobStatus::Running, 2_000),
            // Its last reading before it stopped. Not a current rate.
            job(3, JobStatus::Paused, 9_000),
            job(4, JobStatus::Done, 9_000),
            job(5, JobStatus::Queued, 0),
        ];
        assert_eq!(total_throughput(&jobs), 3_000);
    }

    #[test]
    fn nothing_moving_is_zero_rather_than_a_stale_sum() {
        assert_eq!(total_throughput(&[]), 0);
        assert_eq!(total_throughput(&[job(1, JobStatus::Paused, 9_000)]), 0);
    }

    /// A render pass is not a place to panic, and two jobs near `u64::MAX` are
    /// only reachable through a bad reading — but a bad reading should draw a
    /// wrong number, not take the window down.
    #[test]
    fn an_impossible_reading_saturates_instead_of_overflowing() {
        let jobs = vec![
            job(1, JobStatus::Running, u64::MAX),
            job(2, JobStatus::Running, u64::MAX),
        ];
        assert_eq!(total_throughput(&jobs), u64::MAX);
    }

    #[test]
    fn the_oldest_waiting_job_is_the_one_offered() {
        let jobs = vec![
            job(1, JobStatus::Running, 100),
            job(2, JobStatus::WaitingForInput, 0),
            job(3, JobStatus::WaitingForInput, 0),
        ];
        assert_eq!(first_waiting(&jobs).map(|(id, _)| id), Some(2));
        assert!(first_waiting(&[job(1, JobStatus::Running, 100)]).is_none());
    }
}
