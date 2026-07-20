//! Virtualized list/details view — smooth even with very large directories.

use std::rc::Rc;

use gpui::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement,
    Size, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, h_flex, v_flex, v_virtual_list};

use crate::core::entry::FsEntry;
use crate::core::format::{format_size, format_time};
use crate::ui::components::entry_icon;
use crate::ui::explorer::ExplorerPanel;

pub(super) const ROW_HEIGHT: f32 = 30.;

impl ExplorerPanel {
    pub(super) fn render_list(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sizes: Rc<Vec<Size<gpui::Pixels>>> =
            Rc::new(vec![size(px(100.), px(ROW_HEIGHT)); self.entries.len()]);

        v_flex()
            .size_full()
            .child(
                // Column headers.
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().min_w_0().child("Name"))
                    .child(div().w(px(90.)).flex_none().text_right().child("Size"))
                    .child(div().w(px(140.)).flex_none().text_right().child("Modified")),
            )
            .child(
                div().flex_1().min_h_0().px_1().child(
                    v_virtual_list(
                        cx.entity(),
                        "piku-file-list",
                        sizes,
                        move |this, range, _window, cx| {
                            range
                                .map(|ix| this.render_row(ix, cx))
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.scroll),
                ),
            )
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let entry: FsEntry = entry.clone();
        let selected = self.selected.contains(&ix);
        let dimmed = entry.hidden;

        let size_text = if entry.is_dir() {
            "—".to_string()
        } else {
            format_size(entry.size)
        };

        h_flex()
            .id(ix)
            .w_full()
            .h(px(ROW_HEIGHT))
            .px_2()
            .gap_2()
            .items_center()
            .rounded(px(6.))
            .cursor_pointer()
            .when(selected, |style| {
                style
                    .bg(cx.theme().list_active)
                    .border_1()
                    .border_color(cx.theme().list_active_border)
            })
            .when(!selected, |style| {
                style.hover(|style| style.bg(cx.theme().list_hover))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    window.focus(&this.focus_handle, cx);
                    this.select_only(ix, cx);
                }),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    this.open_entry(ix, window, cx);
                } else {
                    this.click_select(ix, event, window, cx);
                }
            }))
            .child(entry_icon(&entry, cx).size(px(16.)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if dimmed {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(entry.name.clone()),
            )
            .child(
                div()
                    .w(px(90.))
                    .flex_none()
                    .text_right()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(size_text),
            )
            .child(
                div()
                    .w(px(140.))
                    .flex_none()
                    .text_right()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format_time(entry.modified)),
            )
            .into_any_element()
    }
}
