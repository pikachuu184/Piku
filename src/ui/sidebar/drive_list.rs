//! Drive rows with solid-color capacity bars.

use gpui::{App, Div, ParentElement, Styled, div, px, relative};
use gpui_component::{ActiveTheme as _, h_flex, v_flex};

use crate::core::format::format_size;
use crate::services::fs_service::DriveInfo;

pub fn drive_details(drive: &DriveInfo, cx: &App) -> Div {
    let used = drive.total.saturating_sub(drive.available);
    let fraction = if drive.total > 0 {
        (used as f64 / drive.total as f64) as f32
    } else {
        0.0
    };

    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(drive.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} free", format_size(drive.available))),
                ),
        )
        .child(
            div()
                .w_full()
                .h(px(3.))
                .rounded(cx.theme().radius)
                .bg(cx.theme().border)
                .child(
                    div()
                        .h_full()
                        .rounded(cx.theme().radius)
                        .bg(cx.theme().foreground)
                        .w(relative(fraction.clamp(0.0, 1.0))),
                ),
        )
}
