//! The transfer center: every job, grouped by what it is doing.
//!
//! # Why the header lives in the content
//!
//! A dialog's `.title(…)` is built by a closure the dialog layer re-invokes only
//! when `Root` renders — see `Root::render_dialog_layer`. A count computed there
//! would freeze the moment the jobs store notified without `Root` re-rendering,
//! which is most of the time. So the dialog's title is a fixed word and the live
//! header — [`label::center_title`] plus [`label::activity_summary`] — is the
//! first row of the content, inside this entity, which re-renders on its own
//! `cx.observe(&jobs)`.
//!
//! # One virtual list, for the failures
//!
//! The card list is not virtualized and does not need to be: it is bounded by the
//! number of jobs, which is tens, and `Clear finished` trims the history. What can
//! run to tens of thousands of rows is one job's *failure* list, and that is what
//! gets [`v_virtual_list`] — at a fixed pixel height, so it stays bounded inside
//! the scrolling parent (an unbounded virtual list renders every row, which is
//! the bug the preview module's header warns about).
//!
//! Only one card expands at a time, so there is only ever one such list and one
//! scroll handle. `expanded: Option<u64>` is the whole of that state.
//!
//! Theme rules as elsewhere: `cx.theme().*` tokens only, `cx.theme().radius`
//! corners, `.ghost().xsmall()` buttons with tooltips, no shadows, no hues.

use std::rc::Rc;

use gpui::{
    AnyElement, AppContext as _, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, Render, SharedString, Size, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, VirtualListScrollHandle,
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex, v_virtual_list,
};

use crate::app::assets::PikuIcon;
use crate::backend::services::transfer::job::StateGroup;
use crate::services::jobs::{Job, label};
use crate::state::PikuState;
use crate::ui::components::empty_state;
use crate::ui::transfers::card::{DETAIL_ROW_HEIGHT, card, expandable, failure_row, finished_row};

/// Tallest the expanded failure list gets before it scrolls instead of growing.
const DETAIL_MAX: f32 = 176.0;

/// Open the transfer center.
///
/// `on_close` fires for every way out — the close button, Escape, and clicking
/// the overlay — which is what lets `shell.rs` keep one flag that cannot go
/// stale. It is the only caller: the popover's `Details` dispatches
/// `ToggleTransferCenter` rather than coming here directly, so there is one path
/// in and a second press closes the center instead of stacking a second
/// identical one on top of it.
pub fn open(
    on_close: impl Fn(&mut Window, &mut gpui::App) + 'static,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let view = cx.new(TransferCenter::new);
    // `Rc`, not a move: the dialog layer re-invokes this builder on every `Root`
    // render, so everything it captures has to survive being cloned each time.
    let on_close = Rc::new(on_close);
    window.open_dialog(cx, move |dialog, _, _| {
        let view = view.clone();
        let on_close = on_close.clone();
        dialog
            // Static: see the module header. The live count is the content's
            // first row.
            .title(label::TRANSFERS)
            .w(px(600.))
            .overlay_closable(true)
            .on_close(move |_, window, cx| on_close(window, cx))
            .content(move |content, _, _| content.child(view.clone()))
    });
}

pub struct TransferCenter {
    /// Which job's failure list is open. One at a time, which is what keeps this
    /// surface to a single virtual list.
    expanded: Option<u64>,
    scroll: VirtualListScrollHandle,
}

impl TransferCenter {
    fn new(cx: &mut Context<Self>) -> Self {
        let jobs = PikuState::global(cx).jobs.clone();
        cx.observe(&jobs, |_, _, cx| cx.notify()).detach();
        Self {
            expanded: None,
            scroll: VirtualListScrollHandle::new(),
        }
    }

    /// The expanded job's failure list, or `None` for every other card.
    ///
    /// `detail.is_some()` is what [`card`] treats as "expanded", so this being
    /// the only producer of it means the chevron and the list cannot disagree.
    fn detail(&self, job: &Job, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.expanded != Some(job.id) || !expandable(job) {
            return None;
        }
        let rows = job.failures.len();
        let sizes: Rc<Vec<Size<Pixels>>> =
            Rc::new(vec![size(px(100.), px(DETAIL_ROW_HEIGHT)); rows]);
        // A definite height, not `flex_1`: this sits inside a scrolling parent,
        // and a virtual list that cannot measure its viewport renders every row.
        let height = (rows as f32 * DETAIL_ROW_HEIGHT).min(DETAIL_MAX);
        let id = job.id;

        Some(
            div()
                .w_full()
                .h(px(height))
                .child(
                    v_virtual_list(
                        cx.entity(),
                        SharedString::from(format!("job-{id}-failures")),
                        sizes,
                        move |this, range, _window, cx| {
                            range.map(|ix| this.failure(id, ix, cx)).collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.scroll),
                )
                .into_any_element(),
        )
    }

    /// One row of the expanded job's failure list, looked up by job id.
    ///
    /// By id rather than by a captured slice: the list closure outlives the render
    /// that built it, and a job's failures grow while it runs. A stale clone would
    /// render rows the engine has since replaced.
    fn failure(&self, job: u64, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let jobs = PikuState::global(cx).jobs.read(cx);
        let found = jobs
            .jobs()
            .iter()
            .find(|candidate| candidate.id == job)
            .and_then(|job| job.failures.get(index))
            .cloned();
        match found {
            Some(failure) => failure_row(&failure, cx),
            // The job was removed, or its failures were replaced by a retry,
            // between the measure and the paint.
            None => div().h(px(DETAIL_ROW_HEIGHT)).into_any_element(),
        }
    }

    /// One group, or nothing at all when it is empty.
    ///
    /// Empty groups are dropped rather than shown with a zero: four headings over
    /// one card reads as three things having gone wrong.
    fn group(
        &self,
        heading: &'static str,
        jobs: &[Job],
        trailing: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if jobs.is_empty() {
            return None;
        }
        let mut section = v_flex().w_full().gap_1().child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(heading),
                )
                .children(trailing),
        );
        for job in jobs {
            // A finished job gets a line, not a card: there is nothing left to
            // pause, no rate to draw, and no ring worth 26 pixels.
            section = section.child(if job.status.is_terminal() {
                finished_row(job, cx)
            } else {
                let id = job.id;
                let detail = self.detail(job, cx);
                card(
                    job,
                    detail,
                    cx.listener(move |this: &mut Self, _, _, cx| {
                        this.expanded = (this.expanded != Some(id)).then_some(id);
                        cx.notify();
                    }),
                    cx,
                )
            });
        }
        Some(section.into_any_element())
    }
}

impl Render for TransferCenter {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("TransferCenter");
        let store = PikuState::global(cx).jobs.clone();
        let (all, title, summary, can_pause, can_resume) = {
            let jobs = store.read(cx);
            let all = jobs.jobs().to_vec();
            (
                all,
                label::center_title(jobs.active_count()),
                label::activity_summary(jobs.jobs()),
                jobs.can_pause_any(),
                jobs.can_resume_any(),
            )
        };

        // Grouped by what a user would do about them, and by the engine's own
        // grouping rather than a second copy of it: `Pausing` sits with `Running`
        // (it is still moving) and `Paused` does too (it is still this job's turn,
        // and the button that resumes it is on the card).
        let mut active = Vec::new();
        let mut waiting = Vec::new();
        let mut queued = Vec::new();
        let mut finished = Vec::new();
        for job in &all {
            match job.status.group() {
                StateGroup::Active => active.push(job.clone()),
                StateGroup::WaitingForInput => waiting.push(job.clone()),
                StateGroup::Queued => queued.push(job.clone()),
                StateGroup::Completed => finished.push(job.clone()),
            }
        }

        let header = h_flex()
            .w_full()
            // Room for the dialog's absolutely-positioned close button.
            .pr_6()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(summary),
            );

        let mut body = v_flex()
            .id("transfer-center-body")
            .w_full()
            .h(px(420.))
            .gap_3()
            .overflow_y_scroll();

        if all.is_empty() {
            body = body.child(div().w_full().h(px(240.)).child(empty_state(
                Icon::new(IconName::ArrowRight),
                label::CENTER_EMPTY_TITLE,
                label::CENTER_EMPTY_HINT,
                cx,
            )));
        } else {
            // Waiting first: it is the group where nothing happens until a human
            // acts, so it is the one worth finding without scrolling.
            body = body
                .children(self.group(label::GROUP_WAITING, &waiting, None, cx))
                .children(self.group(label::GROUP_ACTIVE, &active, None, cx))
                .children(self.group(label::GROUP_QUEUED, &queued, None, cx))
                .children(
                    self.group(
                        label::GROUP_FINISHED,
                        &finished,
                        Some(
                            Button::new("transfers-clear-finished")
                                .label(label::CLEAR_FINISHED)
                                .xsmall()
                                .ghost()
                                .tooltip("Remove finished transfers from this list")
                                .on_click({
                                    let store = store.clone();
                                    move |_, _, cx| {
                                        store.update(cx, |jobs, cx| jobs.clear_finished(cx));
                                    }
                                })
                                .into_any_element(),
                        ),
                        cx,
                    ),
                );
        }

        v_flex().w_full().gap_2().child(header).child(body).child(
            h_flex()
                .w_full()
                .pt_1()
                .gap_1()
                .items_center()
                .border_t_1()
                .border_color(cx.theme().border)
                // Persistent, not conditional: a control that disappears when
                // it cannot act teaches the user it was never there. Disabled
                // says "not now", which is the truth.
                .child(
                    Button::new("transfers-center-pause-all")
                        .icon(PikuIcon::Pause)
                        .label(label::PAUSE_ALL)
                        .xsmall()
                        .ghost()
                        .disabled(!can_pause)
                        .on_click({
                            let store = store.clone();
                            move |_, _, cx| {
                                store.update(cx, |jobs, cx| jobs.pause_all(cx));
                            }
                        }),
                )
                .child(
                    Button::new("transfers-center-resume-all")
                        .icon(PikuIcon::Play)
                        .label(label::RESUME_ALL)
                        .xsmall()
                        .ghost()
                        .disabled(!can_resume)
                        .on_click({
                            let store = store.clone();
                            move |_, _, cx| {
                                store.update(cx, |jobs, cx| jobs.resume_all(cx));
                            }
                        }),
                )
                .child(div().flex_1())
                .when(!waiting.is_empty(), |row| {
                    let id = waiting[0].id;
                    row.child(
                        Button::new("transfers-center-resolve")
                            .label(label::RESOLVE)
                            .xsmall()
                            .ghost()
                            .tooltip("Answer this transfer's conflicts")
                            .on_click(move |_, window, cx| {
                                crate::ui::transfers::conflicts::ConflictView::open(id, window, cx);
                            }),
                    )
                }),
        )
    }
}
