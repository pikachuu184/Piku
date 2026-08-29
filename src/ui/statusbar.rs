//! Bottom status bar: listing/selection summary on the left, transfer progress
//! and dock toggles on the right.
//!
//! The transfer segment is a click target, exactly like the git segment beside
//! it: both are a summary that opens the panel holding the detail. It names the
//! newest live job rather than aggregating, because a bar is one line and
//! `2 copying · 1 waiting` is what the popover's headline is for.

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement, StatefulInteractiveElement as _,
    Styled, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    dock::DockPlacement,
    h_flex,
    progress::Progress,
    status_bar::StatusBar,
};

use crate::app::assets::PikuIcon;

use crate::core::format::format_size;
use crate::services::jobs::{JobStatus, label};
use crate::state::PikuState;
use crate::ui::shell::Workspace;

pub fn render_status_bar(workspace: &Workspace, cx: &mut Context<Workspace>) -> impl IntoElement {
    let state = PikuState::global(cx);
    let selection = state.selection.read(cx);

    let summary = {
        let items = selection.dir_items;
        let picked = selection.entries.len();
        if picked > 0 {
            let bytes: u64 = selection.entries.iter().map(|e| e.size).sum();
            if bytes > 0 {
                format!("{picked} of {items} selected · {}", format_size(bytes))
            } else {
                format!("{picked} of {items} selected")
            }
        } else {
            format!("{items} items")
        }
    };

    // Git segment: branch, sync arrows, and dirty count for the repository
    // containing the active pane's directory (if any). All strings arriving
    // from the store are pre-sanitized and length-capped.
    struct GitSegment {
        name: String,
        ahead_behind: Option<(usize, usize)>,
        dirty: String,
        in_progress: Option<&'static str>,
    }
    let git_segment = selection.dir.as_deref().and_then(|dir| {
        let git = state.git.read(cx);
        let snap = git.snapshot(git.root_for(dir)?)?;
        let name = snap.branch.clone().or_else(|| {
            snap.detached_short
                .as_ref()
                .map(|s| format!("detached {s}"))
        })?;
        let ahead_behind = match (snap.ahead, snap.behind) {
            (Some(ahead), Some(behind)) if ahead + behind > 0 => Some((ahead, behind)),
            _ => None,
        };
        let dirty = if snap.dirty_total() > 0 {
            format!(
                "· {}{} changed",
                snap.dirty_total(),
                if snap.truncated { "+" } else { "" }
            )
        } else {
            String::new()
        };
        Some(GitSegment {
            name,
            ahead_behind,
            dirty,
            in_progress: snap.in_progress,
        })
    });

    // The newest live job, and how many others are live behind it. Cloned so
    // the borrow of `state` ends here — `cx.listener` below needs it mutably.
    let transfer = {
        let jobs = state.jobs.read(cx);
        jobs.active_job()
            .cloned()
            .map(|job| (job, jobs.active_count()))
    };
    let dock_area = workspace.dock_area().clone();

    let mut left = h_flex()
        .items_center()
        .gap_2()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(summary);
    if let Some(seg) = git_segment {
        // Clickable: opens the branch panel anchored above this segment.
        let mut segment = h_flex()
            .id("status-git-segment")
            .px_1()
            .gap_1()
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().list_hover))
            .child(
                Icon::new(PikuIcon::GitBranch)
                    .size(px(12.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(seg.name);
        if let Some(state) = seg.in_progress {
            segment = segment.child(format!("({state})"));
        }
        if let Some((ahead, behind)) = seg.ahead_behind {
            segment = segment
                .child(
                    Icon::new(IconName::ArrowUp)
                        .size(px(11.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(format!("{ahead}"))
                .child(
                    Icon::new(IconName::ArrowDown)
                        .size(px(11.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(format!("{behind}"));
        }
        if !seg.dirty.is_empty() {
            segment = segment.child(seg.dirty);
        }
        left = left.child(segment.on_click(cx.listener(|workspace, _, window, cx| {
            workspace.toggle_branch_popover(window, cx);
        })));
    }
    let mut bar = StatusBar::new().left(left);

    if let Some((job, live)) = transfer {
        let id = job.id;
        // Present for every live job, not just a running one. A transfer that
        // stopped for an answer is the one worth finding, and hiding it until
        // bytes move again is how it goes unnoticed.
        let mut segment = h_flex()
            .id("status-transfer-segment")
            .px_1()
            .gap_2()
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().list_hover))
            .child(
                Icon::new(IconName::ArrowRight)
                    .size(px(12.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .max_w(px(180.))
                    .truncate()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(job.title.clone()),
            );
        if live > 1 {
            segment = segment.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("+{} more", live - 1)),
            );
        }
        // The state word, whenever it is not the unremarkable one. Waiting gets
        // the brightest tier — the grayscale emphasis rule the git badges use —
        // because it is the only state here that does not continue on its own.
        if job.status != JobStatus::Running {
            segment = segment.child(
                div()
                    .text_xs()
                    .text_color(if job.status == JobStatus::WaitingForInput {
                        cx.theme().foreground
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(label::status_label(&job.status)),
            );
        }
        // Only once something has been counted. A bar at zero on a job still
        // walking the tree reads as a stall rather than as an unknown size.
        if job.total_bytes > 0 || job.total_items > 0 {
            segment = segment.child(
                div()
                    .w(px(110.))
                    .child(Progress::new("job-progress").value(job.percent())),
            );
        }

        bar = bar.right(
            h_flex()
                .items_center()
                .gap_1()
                .child(segment.on_click(cx.listener(|workspace, _, _, cx| {
                    workspace.toggle_transfer_popover(cx);
                })))
                // A sibling of the click target, not a child of it: cancelling
                // and opening the popover are different intents, and one click
                // must not do both.
                .child(
                    Button::new("cancel-job")
                        .icon(IconName::Close)
                        .xsmall()
                        .ghost()
                        .tooltip("Cancel")
                        // By id, not `cancel_active`: this button names the job
                        // the segment named, and the newest live job may have
                        // changed between the paint and the click.
                        .on_click(cx.listener(move |_, _, _, cx| {
                            PikuState::global(cx)
                                .jobs
                                .clone()
                                .update(cx, |jobs, cx| jobs.cancel(id, cx));
                        })),
                ),
        );
    }

    bar.right(
        h_flex()
            .items_center()
            .gap_0p5()
            .child(
                Button::new("status-left-dock")
                    .icon(IconName::PanelLeft)
                    .xsmall()
                    .ghost()
                    .tooltip("Toggle navigation (Ctrl+B)")
                    .on_click(cx.listener({
                        let dock_area = dock_area.clone();
                        move |_, _, window, cx| {
                            dock_area.update(cx, |dock_area, cx| {
                                dock_area.toggle_dock(DockPlacement::Left, window, cx);
                            });
                        }
                    })),
            )
            .child(
                Button::new("status-right-dock")
                    .icon(IconName::PanelRight)
                    .xsmall()
                    .ghost()
                    .tooltip("Toggle details (Ctrl+Alt+B)")
                    .on_click(cx.listener({
                        let dock_area = dock_area.clone();
                        move |_, _, window, cx| {
                            dock_area.update(cx, |dock_area, cx| {
                                dock_area.toggle_dock(DockPlacement::Right, window, cx);
                            });
                        }
                    })),
            ),
    )
}
