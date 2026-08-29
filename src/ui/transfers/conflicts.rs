//! The one surface that settles collisions, and the only place a
//! [`Decision`] is built.
//!
//! # One decision per ask
//!
//! [`TransferControl::arm_decision`](crate::backend::services::transfer::control::TransferControl::arm_decision)
//! installs a fresh one-shot for every stop, and `deliver` *takes* the sender —
//! so the second `resolve` for one ask returns `false` and is dropped on the
//! floor. Everything below follows from that: this view sends exactly one
//! `Decision`, and [`decision_for`] is the pure function that decides whether one
//! can be built yet.
//!
//! The consequence users see is in the mixed case. When a job is stopped by both
//! a whole-job block (a space shortfall, a refused path) and a set of name
//! clashes, per-row answers cannot settle it: `Decision::Each` deliberately
//! leaves the block in place, and there is no second delivery to clear it with.
//! `Decision::All` is the only answer that does both, so in that case the blanket
//! checkbox is forced on and [`label::CONFLICT_ALL_REQUIRED`] says why. A greyed
//! button with no reason is how a user concludes the app is broken.
//!
//! # Positional, and re-read every notify
//!
//! `Decision::Each` is positional against
//! `pending.iter().filter(Conflict::is_per_entry)` — the engine zips the two and
//! returns [`Outcome::Incomplete`](crate::backend::services::transfer::conflict::Outcome)
//! on a length mismatch rather than answering the wrong files. So [`choices`]
//! carries one slot per clash in that exact order, and [`ConflictView::sync`]
//! rebuilds the whole list if the engine's `pending` ever differs from the
//! snapshot this view was opened on. Half-answering a list that moved underneath
//! is the failure this guards against.
//!
//! # What this view never holds
//!
//! No `PathBuf`, and no raw string from the filesystem. Every name and path on
//! screen arrives pre-sanitized inside [`NameClash`] — the engine passed them
//! through `security::text` before they left the worker — and a row names its
//! entry back to the engine by *index*, never by path. A typed rename is the one
//! piece of new text here, and it goes through
//! [`FileName::new`](crate::backend::path::FileName::new) — the same validator
//! the rename dialog uses — before it can become a policy.
//!
//! Theme rules as elsewhere on this surface: `cx.theme().*` tokens only,
//! `cx.theme().radius` corners, `.ghost().xsmall()` buttons, no shadows, no hues.

use std::rc::Rc;

use gpui::{
    AnyElement, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Pixels, Render,
    SharedString, Size, Styled as _, Window, div, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
    v_flex, v_virtual_list,
};
use gpui_component::{VirtualListScrollHandle, WindowExt as _};

use crate::backend::path::FileName;
use crate::backend::services::transfer::conflict::{Conflict, ConflictPolicy, Decision, NameClash};
use crate::security::text::sanitize_label;
use crate::services::jobs::label;
use crate::state::PikuState;

/// Height of one conflict row, in pixels: four lines of `text_xs` plus the
/// button row, plus [`ROW_GAP`].
const ROW_HEIGHT: f32 = 88.0;

/// Vertical breathing room between two bordered rows, taken out of
/// [`ROW_HEIGHT`] so the virtual list's offsets stay whole.
const ROW_GAP: f32 = 6.0;

/// Tallest the scrolling list gets before it starts scrolling instead of growing.
/// A 40 000-conflict job must not produce a 40 000-row dialog.
const LIST_MAX: f32 = 320.0;

/// What the `Apply` button would send, or `None` while the answer is incomplete.
///
/// Pure, and separated from the view because this is the part that has to be
/// right: every rule it encodes is one the engine enforces on the other side, and
/// getting one wrong means either a silently dropped answer or the wrong files
/// overwritten.
///
/// - No per-entry clashes: the only thing to answer is the block, and
///   [`Decision::Proceed`] is what clears it.
/// - `apply_all`: one blanket policy for everything, which is also the only
///   answer that clears a block. `Rename` is refused because the engine refuses
///   it — eight clashes renamed to one name would collide into one file.
/// - Otherwise: every row must have an answer, and a whole-job block must not be
///   present, because `Each` cannot clear one and there is no second delivery.
fn decision_for(
    pending: &[Conflict],
    choices: &[Option<ConflictPolicy>],
    apply_all: bool,
) -> Option<Decision> {
    let entries = pending.iter().filter(|c| c.is_per_entry()).count();
    let blocked = pending.len() > entries;

    if entries == 0 {
        // An empty `pending` is not something to answer at all; a block alone is.
        return blocked.then_some(Decision::Proceed);
    }
    if choices.len() != entries {
        // The rows and the engine's list disagree about what was shown. `Each` is
        // positional, so sending anything now would answer the wrong files.
        return None;
    }

    if apply_all {
        let mut answered = choices.iter().flatten();
        let first = answered.next()?;
        if first.asks() || matches!(first, ConflictPolicy::Rename(_)) {
            return None;
        }
        // Every row carries the same pick in this mode. Disagreement means the
        // state drifted, and picking one of two policies is not what was shown.
        if answered.any(|policy| policy != first) {
            return None;
        }
        return Some(Decision::All(first.clone()));
    }

    if blocked {
        return None;
    }
    let picks: Vec<ConflictPolicy> = choices.iter().cloned().collect::<Option<_>>()?;
    picks
        .iter()
        .all(|policy| !policy.asks())
        .then_some(Decision::Each(picks))
}

/// The conflict surface for one job.
pub struct ConflictView {
    job: u64,
    /// The engine's list, verbatim and in order — what a [`Decision::Each`] is
    /// positional against.
    pending: Vec<Conflict>,
    /// The per-entry half of `pending`, in the same order, for rendering.
    clashes: Vec<NameClash>,
    /// The whole-job half: rendered above the list, answered by `Proceed`.
    blocks: Vec<Conflict>,
    /// One slot per entry of `clashes`. `None` means unanswered.
    choices: Vec<Option<ConflictPolicy>>,
    /// `true` when one policy covers every row. Forced on — and the checkbox
    /// disabled — when a whole-job block sits beside the clashes.
    apply_all: bool,
    /// Which row has its rename field open, as an index into `clashes`.
    renaming: Option<usize>,
    rename: Entity<InputState>,
    /// A rejected rename, already sanitized for display.
    error: Option<SharedString>,
    /// Set once the job stops waiting — cancelled elsewhere, or answered from
    /// another window. The buttons go away rather than delivering into a closed
    /// channel.
    stale: bool,
    scroll: VirtualListScrollHandle,
}

impl ConflictView {
    /// Open the surface for `job`, or do nothing if it is not waiting on one.
    ///
    /// Deliberately not automatic. A modal that appears on its own while the user
    /// is typing somewhere else steals the keystroke that dismisses it; the card
    /// says `Waiting for input` and offers this, which is discoverable without
    /// taking the window away from whatever is in front of it.
    pub fn open(job: u64, window: &mut Window, cx: &mut gpui::App) {
        let pending = {
            let jobs = PikuState::global(cx).jobs.read(cx);
            match jobs.jobs().iter().find(|candidate| candidate.id == job) {
                Some(found) if !found.pending.is_empty() => found.pending.clone(),
                // Nothing to decide. Opening an empty decision surface would
                // invite an answer the engine is not listening for.
                _ => return,
            }
        };

        let view = cx.new(|cx| Self::new(job, pending, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            let view = view.clone();
            dialog
                // Static, on purpose. The dialog layer re-runs this builder only
                // when `Root` renders, so a count computed here would freeze
                // while the engine kept re-asking; the live count is the first
                // line of the content, which re-renders on its own notify.
                .title(label::CONFLICT_TITLE)
                .w(px(560.))
                // A stray click outside must not throw away eight answers. The
                // close button still dismisses, which leaves the job waiting —
                // the card reopens this.
                .overlay_closable(false)
                .content(move |content, _, _| content.child(view.clone()))
        });
    }

    fn new(job: u64, pending: Vec<Conflict>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let rename = cx.new(|cx| InputState::new(window, cx).placeholder("New name…"));
        let jobs = PikuState::global(cx).jobs.clone();
        // The engine can re-ask with a different list — a file created mid-scan —
        // and it can finish or be cancelled from another surface. Either way the
        // snapshot below stops being what a decision would be measured against.
        cx.observe(&jobs, |this: &mut Self, _, cx| this.sync(cx))
            .detach();

        let mut this = Self {
            job,
            pending: Vec::new(),
            clashes: Vec::new(),
            blocks: Vec::new(),
            choices: Vec::new(),
            apply_all: false,
            renaming: None,
            rename,
            error: None,
            stale: false,
            scroll: VirtualListScrollHandle::new(),
        };
        this.adopt(pending);
        this
    }

    /// Replace the snapshot, discarding answers that were made against the old
    /// one. Dropping them is the point: a `Replace` chosen for row 3 of the
    /// previous list is not an answer about row 3 of this one.
    fn adopt(&mut self, pending: Vec<Conflict>) {
        self.clashes = pending
            .iter()
            .filter_map(|conflict| match conflict {
                Conflict::NameTaken(clash) => Some(clash.clone()),
                _ => None,
            })
            .collect();
        self.blocks = pending
            .iter()
            .filter(|conflict| !conflict.is_per_entry())
            .cloned()
            .collect();
        self.choices = vec![None; self.clashes.len()];
        // See the module header: only `All` clears a block *and* the clashes, and
        // there is one delivery to do it in.
        self.apply_all = self.blanket_required();
        self.renaming = None;
        self.error = None;
        self.pending = pending;
    }

    /// Re-read the job, and rebuild if the engine's list moved.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let jobs = PikuState::global(cx).jobs.clone();
        let pending = jobs
            .read(cx)
            .jobs()
            .iter()
            .find(|candidate| candidate.id == self.job)
            .map(|job| job.pending.clone());

        match pending {
            Some(pending) if pending == self.pending => {}
            Some(pending) if pending.is_empty() => self.stale = true,
            Some(pending) => {
                self.stale = false;
                self.adopt(pending);
            }
            // The job left the store entirely — removed from history while this
            // was open.
            None => self.stale = true,
        }
        cx.notify();
    }

    /// Whether one answer has to cover everything. See the module header.
    fn blanket_required(&self) -> bool {
        !self.blocks.is_empty() && !self.clashes.is_empty()
    }

    /// Record `policy` for row `index`, or for every row in blanket mode.
    fn choose(&mut self, index: usize, policy: ConflictPolicy, cx: &mut Context<Self>) {
        if self.apply_all {
            for slot in &mut self.choices {
                *slot = Some(policy.clone());
            }
        } else if let Some(slot) = self.choices.get_mut(index) {
            *slot = Some(policy);
        }
        self.renaming = None;
        self.error = None;
        cx.notify();
    }

    /// Open the rename field for one row, seeded with the free name `Keep Both`
    /// would have used — a valid [`FileName`] by construction, and a sensible
    /// thing to edit rather than an empty box.
    fn start_rename(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(clash) = self.clashes.get(index) else {
            return;
        };
        let seed = clash.keep_both.to_string();
        self.rename
            .update(cx, |state, cx| state.set_value(seed, window, cx));
        self.renaming = Some(index);
        self.error = None;
        cx.notify();
    }

    /// Validate the typed name and, if it holds, make it this row's answer.
    fn commit_rename(&mut self, index: usize, cx: &mut Context<Self>) {
        let typed = self.rename.read(cx).value().trim().to_string();
        match FileName::new(&typed) {
            Ok(name) => self.choose(index, ConflictPolicy::Rename(name), cx),
            Err(error) => {
                // Through the sanitizer: `PathError::InvalidName` embeds the
                // rejected text, and the reason a name was rejected can be that
                // it carries something that must not be rendered raw.
                self.error = Some(sanitize_label(&error.to_string()).into());
                cx.notify();
            }
        }
    }

    /// Deliver one decision and close.
    fn send(&mut self, decision: Decision, window: &mut Window, cx: &mut Context<Self>) {
        let job = self.job;
        let jobs = PikuState::global(cx).jobs.clone();
        jobs.update(cx, |jobs, cx| jobs.resolve(job, decision, cx));
        window.close_dialog(cx);
    }

    fn row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(clash) = self.clashes.get(index).cloned() else {
            return div().h(px(ROW_HEIGHT)).into_any_element();
        };
        let job = self.job;
        let chosen = self.choices.get(index).cloned().flatten();
        let renaming = self.renaming == Some(index);
        let summary = label::conflict_summary(&Conflict::NameTaken(clash.clone()));

        // A named answer for one row cannot be part of a blanket answer — see
        // `decision_for` — so the button is not offered in that mode.
        let rename_allowed = !self.apply_all;
        let policy_button = |id: &str, text: &'static str, policy: ConflictPolicy| {
            let selected = chosen.as_ref() == Some(&policy);
            Button::new(SharedString::from(format!("conflict-{job}-{index}-{id}")))
                .label(text)
                .xsmall()
                .ghost()
                .selected(selected)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.choose(index, policy.clone(), cx);
                }))
        };

        let mut actions = h_flex().w_full().gap_1().items_center();
        if renaming {
            actions = actions
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(&self.rename).small()),
                )
                .child(
                    Button::new(SharedString::from(format!("conflict-{job}-{index}-save")))
                        .label(label::SAVE)
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, cx| this.commit_rename(index, cx))),
                )
                .child(
                    Button::new(SharedString::from(format!(
                        "conflict-{job}-{index}-abort-rename"
                    )))
                    .icon(IconName::Close)
                    .xsmall()
                    .ghost()
                    .tooltip("Keep asking")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.renaming = None;
                        cx.notify();
                    })),
                );
        } else {
            actions = actions
                .child(policy_button(
                    "replace",
                    label::REPLACE,
                    ConflictPolicy::Replace,
                ))
                .child(policy_button("skip", label::SKIP, ConflictPolicy::Skip))
                .child(policy_button(
                    "keep-both",
                    label::KEEP_BOTH,
                    ConflictPolicy::KeepBoth,
                ))
                .when(rename_allowed, |row| {
                    row.child(
                        Button::new(SharedString::from(format!("conflict-{job}-{index}-rename")))
                            .label(label::RENAME)
                            .xsmall()
                            .ghost()
                            .selected(matches!(chosen, Some(ConflictPolicy::Rename(_))))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.start_rename(index, window, cx);
                            })),
                    )
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        // What the chosen answer will actually write, when the
                        // answer is one that changes the name.
                        .child(match &chosen {
                            Some(ConflictPolicy::Rename(name)) => {
                                format!("Saves it as “{name}”")
                            }
                            _ => label::keep_both_hint(&clash),
                        }),
                );
        }

        div()
            .h(px(ROW_HEIGHT))
            .w_full()
            .pb(px(ROW_GAP))
            .child(
                v_flex()
                    .h_full()
                    .w_full()
                    .px_2()
                    .py_1()
                    .gap_0p5()
                    .justify_center()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .overflow_hidden()
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().foreground)
                            .child(summary),
                    )
                    .child(
                        // The resolved destination: where the bytes land if the
                        // answer is `Replace`.
                        div()
                            .w_full()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(clash.existing_path.to_string()),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(div().flex_1().min_w_0().truncate().child(format!(
                                "{} {}",
                                label::INCOMING,
                                label::side_label(&clash.source)
                            )))
                            .child(div().flex_1().min_w_0().truncate().child(format!(
                                "{} {}",
                                label::EXISTING,
                                label::side_label(&clash.existing)
                            ))),
                    )
                    .child(actions),
            )
            .into_any_element()
    }

    /// A whole-job block, above the list: no per-row answer applies to it.
    fn block(&self, conflict: &Conflict, cx: &Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .child(
                Icon::new(IconName::TriangleAlert)
                    .size(px(12.))
                    .flex_none()
                    // Grayscale emphasis: the brightest tier, not a colour.
                    .text_color(cx.theme().foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(cx.theme().foreground)
                    .child(label::conflict_summary(conflict)),
            )
            .into_any_element()
    }

    /// How many rows still have no answer.
    fn unanswered(&self) -> usize {
        self.choices.iter().filter(|slot| slot.is_none()).count()
    }
}

impl Render for ConflictView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("ConflictView");
        let rows = self.clashes.len();
        let sizes: Rc<Vec<Size<Pixels>>> = Rc::new(vec![size(px(100.), px(ROW_HEIGHT)); rows]);
        let list_height = (rows as f32 * ROW_HEIGHT).min(LIST_MAX);
        let forced = self.blanket_required();
        let decision = decision_for(&self.pending, &self.choices, self.apply_all);
        let unanswered = self.unanswered();

        let mut surface = v_flex().w_full().gap_2();

        // The heading again, inside the content: the dialog's own title is built
        // by a closure the dialog layer only re-runs when `Root` renders, so a
        // count that changes on a jobs notify goes stale up there.
        surface = surface.child(
            div()
                .w_full()
                // Room for the dialog's absolutely-positioned close button.
                .pr_6()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(label::conflict_heading(self.pending.len())),
        );

        if self.stale {
            return surface
                .child(
                    div()
                        .w_full()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label::NOT_WAITING),
                )
                .into_any_element();
        }

        for block in &self.blocks {
            surface = surface.child(self.block(block, cx));
        }
        if rows > 0 {
            surface = surface.child(
                div().w_full().h(px(list_height)).child(
                    v_virtual_list(
                        cx.entity(),
                        SharedString::from(format!("conflict-rows-{}", self.job)),
                        sizes,
                        move |this, range, _window, cx| {
                            range.map(|ix| this.row(ix, cx)).collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.scroll),
                ),
            );
        }

        if let Some(error) = self.error.clone() {
            surface = surface.child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().foreground)
                    .child(Icon::new(IconName::TriangleAlert).size(px(12.)).flex_none())
                    .child(error),
            );
        }
        if forced {
            surface = surface.child(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label::CONFLICT_ALL_REQUIRED),
            );
        }

        // Footer.
        let apply_label = if rows == 0 {
            label::CONTINUE_ANYWAY
        } else {
            label::APPLY
        };
        let mut footer = h_flex()
            .w_full()
            .pt_1()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border);
        if rows > 0 {
            footer = footer.child(
                Checkbox::new("conflict-apply-all")
                    .label(label::APPLY_TO_ALL)
                    .checked(self.apply_all)
                    .disabled(forced)
                    .when(forced, |checkbox| {
                        checkbox.tooltip(label::CONFLICT_ALL_REQUIRED)
                    })
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.apply_all = *checked;
                        // Answers made row by row are not a blanket answer, and a
                        // blanket answer is not eight individual ones. Either way
                        // what was picked no longer describes what is selected.
                        this.choices.iter_mut().for_each(|slot| *slot = None);
                        this.renaming = None;
                        this.error = None;
                        cx.notify();
                    })),
            );
        }
        footer = footer.child(div().flex_1());
        if unanswered > 0 && decision.is_none() {
            footer = footer.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label::conflict_remaining(unanswered)),
            );
        }
        footer = footer
            .child(
                Button::new("conflict-cancel")
                    .label(label::CANCEL_TRANSFER)
                    .xsmall()
                    .ghost()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.send(Decision::Cancel, window, cx);
                    })),
            )
            .child(
                Button::new("conflict-apply")
                    .label(apply_label)
                    .xsmall()
                    .primary()
                    .disabled(decision.is_none())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        // Rebuilt from current state rather than captured: the
                        // click and the render that produced it are separate
                        // turns, and a notify in between could have moved the
                        // list underneath.
                        if let Some(decision) =
                            decision_for(&this.pending, &this.choices, this.apply_all)
                        {
                            this.send(decision, window, cx);
                        }
                    })),
            );

        surface.child(footer).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::services::transfer::conflict::Side;
    use std::sync::Arc;

    fn clash(index: usize) -> Conflict {
        Conflict::NameTaken(NameClash {
            index,
            relative: Arc::from(format!("file-{index}.txt")),
            existing_path: Arc::from(format!("/backup/file-{index}.txt")),
            source: Side::default(),
            existing: Side::default(),
            keep_both: Arc::from(format!("file-{index} (2).txt")),
        })
    }

    fn shortfall() -> Conflict {
        Conflict::InsufficientSpace {
            needed: 1024,
            available: 0,
        }
    }

    #[test]
    fn every_row_needs_an_answer_before_each_can_be_sent() {
        let pending = vec![clash(0), clash(1)];
        assert_eq!(
            decision_for(&pending, &[Some(ConflictPolicy::Replace), None], false),
            None
        );
        assert_eq!(
            decision_for(
                &pending,
                &[Some(ConflictPolicy::Replace), Some(ConflictPolicy::Skip)],
                false
            ),
            Some(Decision::Each(vec![
                ConflictPolicy::Replace,
                ConflictPolicy::Skip
            ]))
        );
    }

    #[test]
    fn a_blanket_answer_is_one_policy_for_the_whole_list() {
        let pending = vec![clash(0), clash(1), clash(4)];
        let choices = vec![Some(ConflictPolicy::KeepBoth); 3];
        assert_eq!(
            decision_for(&pending, &choices, true),
            Some(Decision::All(ConflictPolicy::KeepBoth))
        );
    }

    /// `ConflictResolver::record` returns `Incomplete` for `All(Rename(_))` —
    /// eight clashes renamed to one name would collide into one file. The UI must
    /// not be able to send it.
    #[test]
    fn a_blanket_rename_is_never_offered() {
        let pending = vec![clash(0), clash(1)];
        let name = ConflictPolicy::Rename(FileName::new("kept.txt").unwrap());
        // `vec!`, not `[_; 2]`: `Rename` carries a `FileName`, so the policy is
        // `Clone` and not `Copy`.
        let choices = vec![Some(name.clone()); 2];
        assert_eq!(decision_for(&pending, &choices, true), None);
        // Per row it is a perfectly good answer.
        assert!(matches!(
            decision_for(&pending, &[Some(name), Some(ConflictPolicy::Skip)], false),
            Some(Decision::Each(_))
        ));
    }

    #[test]
    fn a_shortfall_on_its_own_is_answered_by_proceeding() {
        assert_eq!(
            decision_for(&[shortfall()], &[], false),
            Some(Decision::Proceed)
        );
        assert_eq!(
            decision_for(&[shortfall()], &[], true),
            Some(Decision::Proceed)
        );
    }

    /// The mixed case, and the reason the checkbox is forced on: `Each` leaves
    /// `overridden` alone, and `deliver` takes the one-shot, so there is no
    /// second `resolve` to clear the block with.
    #[test]
    fn a_shortfall_beside_clashes_can_only_be_answered_all_at_once() {
        let pending = vec![shortfall(), clash(0), clash(1)];
        let answered = vec![Some(ConflictPolicy::Replace); 2];
        assert_eq!(decision_for(&pending, &answered, false), None);
        assert_eq!(
            decision_for(&pending, &answered, true),
            Some(Decision::All(ConflictPolicy::Replace))
        );
    }

    /// A list length that does not match is the engine and the UI disagreeing
    /// about what was shown. `record` refuses it; so does this, rather than
    /// answering the wrong files.
    #[test]
    fn a_choice_list_that_does_not_line_up_sends_nothing() {
        let pending = vec![clash(0), clash(1)];
        assert_eq!(
            decision_for(&pending, &[Some(ConflictPolicy::Replace)], false),
            None
        );
        let too_many = vec![Some(ConflictPolicy::Replace); 3];
        assert_eq!(decision_for(&pending, &too_many, true), None);
    }

    #[test]
    fn nothing_pending_is_nothing_to_send() {
        assert_eq!(decision_for(&[], &[], false), None);
        assert_eq!(decision_for(&[], &[], true), None);
    }

    /// Blanket mode reads the first answered slot and requires the rest to agree.
    /// Disagreement means the state drifted, and one of two policies is not what
    /// the user was shown.
    #[test]
    fn a_blanket_answer_will_not_pick_between_two_policies() {
        let pending = vec![clash(0), clash(1)];
        assert_eq!(
            decision_for(
                &pending,
                &[Some(ConflictPolicy::Replace), Some(ConflictPolicy::Skip)],
                true
            ),
            None
        );
        // An unanswered slot is not a disagreement; the mode fills them together.
        assert_eq!(
            decision_for(&pending, &[Some(ConflictPolicy::Replace), None], true),
            Some(Decision::All(ConflictPolicy::Replace))
        );
    }
}
