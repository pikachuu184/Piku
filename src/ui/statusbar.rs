//! Bottom status bar: listing/selection summary on the left, background job
//! progress and dock toggles on the right.

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, div, px,
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
use crate::services::jobs::JobStatus;
use crate::state::PikuState;
use crate::ui::shell::Workspace;

pub fn render_status_bar(
    workspace: &Workspace,
    cx: &mut Context<Workspace>,
) -> impl IntoElement {
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
        let name = snap
            .branch
            .clone()
            .or_else(|| snap.detached_short.as_ref().map(|s| format!("detached {s}")))?;
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

    let job = state.jobs.read(cx).active_job().cloned();
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

    if let Some(job) = job
        && job.status == JobStatus::Running {
            bar = bar.right(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(job.title.clone()),
                    )
                    .child(div().w(px(110.)).child(Progress::new("job-progress").value(job.percent())))
                    .child(
                        Button::new("cancel-job")
                            .icon(IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip("Cancel")
                            .on_click(cx.listener(|_, _, _, cx| {
                                PikuState::global(cx)
                                    .jobs
                                    .clone()
                                    .update(cx, |jobs, cx| jobs.cancel_active(cx));
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
