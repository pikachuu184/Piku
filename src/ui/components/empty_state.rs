//! Shared placeholder for empty folders, empty selections, and error states.

use gpui::{App, Div, ParentElement, SharedString, Styled, px};
use gpui_component::{ActiveTheme as _, Icon, v_flex};

pub fn empty_state(
    icon: Icon,
    title: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    cx: &App,
) -> Div {
    let title: SharedString = title.into();
    let hint: SharedString = hint.into();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .child(icon.size(px(44.)).text_color(cx.theme().muted_foreground))
        .child(
            v_flex()
                .items_center()
                .gap_1()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(title),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(hint),
                ),
        )
}
