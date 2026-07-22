//! Grid view: icon tiles with two-line labels.

use gpui::{
    AppContext as _, ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement, SharedString, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme as _, Sizable as _, v_flex};

use super::explorer_panel::{DragPreview, DraggedPaths};
use crate::ui::components::entry_visual;
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
        let renaming = self.renaming_ix() == Some(ix);
        let editor = if renaming {
            self.inline_edit.as_ref().map(|edit| edit.input.clone())
        } else {
            None
        };

        let drag_paths = self.drag_paths(ix);
        let drag_label: SharedString = if drag_paths.len() > 1 {
            format!("{} items", drag_paths.len()).into()
        } else {
            entry.name.clone().into()
        };
        let source_id = self.session_id();
        let is_dir = entry.is_dir();
        let dir_dest = entry.path.clone();

        v_flex()
            .id(SharedString::from(format!("tile-{ix}")))
            .w(px(104. * zoom))
            .h(px(96. * zoom))
            .p_2()
            .gap_1()
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            // While renaming, the tile's active border/bg are suppressed so the
            // editor's own rounded 6px chrome is the single focus ring.
            .when(selected && !renaming, |style| {
                style
                    .bg(cx.theme().list_active)
                    .border_1()
                    .border_color(cx.theme().list_active_border)
            })
            .when(!selected && !renaming, |style| {
                style.hover(|style| style.bg(cx.theme().list_hover))
            })
            .when(!renaming, |tile| {
                tile.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, _, window, cx| {
                        window.focus(&this.focus_handle, cx);
                        this.select_only(ix, cx);
                    }),
                )
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    if event.click_count() >= 2 {
                        this.open_or_preview_entry(ix, window, cx);
                    } else {
                        this.click_select(ix, event, window, cx);
                    }
                }))
                .when(!drag_paths.is_empty(), |tile| {
                    tile.on_drag(
                        DraggedPaths {
                            paths: drag_paths.clone(),
                            source_id: source_id.clone(),
                        },
                        move |_, _, _, cx| {
                            cx.new(|_| DragPreview {
                                label: drag_label.clone(),
                            })
                        },
                    )
                })
                .when(is_dir, |tile| {
                    tile.drag_over::<DraggedPaths>(|style, _, _, cx| {
                        style.bg(cx.theme().drop_target)
                    })
                    .on_drop(cx.listener({
                        let dest = dir_dest.clone();
                        move |this, dragged: &DraggedPaths, window, cx| {
                            let is_move = !window.modifiers().control;
                            this.drop_into(
                                dragged.paths.clone(),
                                dest.clone(),
                                is_move,
                                window,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let dest = dir_dest.clone();
                        move |this, paths: &gpui::ExternalPaths, window, cx| {
                            this.drop_external(paths.paths().to_vec(), dest.clone(), window, cx);
                        }
                    }))
                })
            })
            .child(
                // Icon plus a git badge in the tile's top-right corner —
                // same data source as the list view's badge column.
                div()
                    .relative()
                    .child(entry_visual(&entry, 34. * zoom, cx))
                    .children(
                        self.git_badge(&entry, cx)
                            .map(|badge| div().absolute().top_0().right_0().child(badge)),
                    ),
            )
            .child(match editor {
                Some(input) => div()
                    .w_full()
                    .rounded(cx.theme().radius)
                    .on_key_down(cx.listener(ExplorerPanel::on_editor_key_down))
                    .child(gpui_component::input::Input::new(&input).small())
                    .into_any_element(),
                None => div()
                    .w_full()
                    .text_size(px(12. * zoom))
                    .text_center()
                    .line_clamp(2)
                    .text_color(if entry.hidden {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(name)
                    .into_any_element(),
            })
            .into_any_element()
    }
}
