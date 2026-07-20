//! Bottom status bar: listing/selection summary on the left, background job
//! progress and dock toggles on the right.

use gpui::{Context, IntoElement, ParentElement, Styled, div, px};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    dock::DockPlacement,
    h_flex,
    progress::Progress,
    status_bar::StatusBar,
};

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

    let job = state.jobs.read(cx).active_job().cloned();
    let dock_area = workspace.dock_area().clone();

    let mut bar = StatusBar::new().left(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(summary),
    );

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
