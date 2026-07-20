//! Collapsible sidebar section with a chevron header.

use gpui::{
    App, ClickEvent, Div, InteractiveElement as _, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex, v_flex};

pub fn section(
    id: &'static str,
    title: &'static str,
    open: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    content: Div,
    cx: &App,
) -> Div {
    let chevron = if open {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    v_flex()
        .w_full()
        .child(
            h_flex()
                .id(id)
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .mx_1()
                .rounded(cx.theme().radius)
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().sidebar_accent))
                .on_click(on_toggle)
                .child(
                    Icon::new(chevron)
                        .size(px(14.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(title.to_uppercase()),
                ),
        )
        .when_open(open, content)
}

trait WhenOpen {
    fn when_open(self, open: bool, content: Div) -> Self;
}

impl WhenOpen for Div {
    fn when_open(self, open: bool, content: Div) -> Self {
        if open { self.child(content) } else { self }
    }
}
