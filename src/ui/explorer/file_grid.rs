//! Grid view: icon tiles with two-line labels.

use gpui::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement,
    SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme as _, v_flex};

use crate::ui::components::entry_icon;
use crate::ui::explorer::ExplorerPanel;

impl ExplorerPanel {
    pub(super) fn render_grid(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tiles = (0..self.entries.len())
            .map(|ix| self.render_tile(ix, cx))
            .collect::<Vec<_>>();

        div()
            .id("piku-file-grid")
            .size_full()
            .overflow_y_scroll()
            .child(div().flex().flex_wrap().gap_1().p_2().children(tiles))
    }

    fn render_tile(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let entry = entry.clone();
        let selected = self.selected.contains(&ix);
        let name: SharedString = entry.name.clone().into();
        let zoom = self.zoom();

        v_flex()
            .id(SharedString::from(format!("tile-{ix}")))
            .w(px(104. * zoom))
            .h(px(96. * zoom))
            .p_2()
            .gap_1()
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
            .child(entry_icon(&entry, cx).size(px(34. * zoom)))
            .child(
                div()
                    .w_full()
                    .text_size(px(12. * zoom))
                    .text_center()
                    .line_clamp(2)
                    .text_color(if entry.hidden {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(name),
            )
            .into_any_element()
    }
}
