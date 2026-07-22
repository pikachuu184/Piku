//! Virtualized list/details view — smooth even with very large directories.

use std::rc::Rc;

use gpui::{
    AppContext as _, ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement, Size, StatefulInteractiveElement as _, Styled, Window, div,
    prelude::FluentBuilder as _, px, size,
};
use gpui_component::{ActiveTheme as _, Sizable as _, h_flex, v_flex, v_virtual_list};

use super::explorer_panel::{DragPreview, DraggedPaths};
use crate::core::entry::FsEntry;
use crate::core::format::{format_size, format_time};
use crate::ui::components::entry_visual;
use crate::ui::explorer::ExplorerPanel;

const BASE_ROW_HEIGHT: f32 = 30.;
const BASE_SIZE_COL: f32 = 90.;
const BASE_DATE_COL: f32 = 140.;

/// List row height at the given zoom level (whole pixels so virtual-list
/// offsets stay crisp).
pub(super) fn row_height(zoom: f32) -> f32 {
    (BASE_ROW_HEIGHT * zoom).round()
}

impl ExplorerPanel {
    pub(super) fn render_list(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let zoom = self.zoom();
        let sizes: Rc<Vec<Size<gpui::Pixels>>> = Rc::new(vec![
            size(px(100.), px(row_height(zoom)));
            self.entries.len()
        ]);

        v_flex()
            .size_full()
            .child(
                // Column headers — widths must scale exactly like the rows so
                // the columns stay aligned at every zoom level.
                h_flex()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().min_w_0().child("Name"))
                    .child(
                        div()
                            .w(px(BASE_SIZE_COL * zoom))
                            .flex_none()
                            .text_right()
                            .child("Size"),
                    )
                    .child(
                        div()
                            .w(px(BASE_DATE_COL * zoom))
                            .flex_none()
                            .text_right()
                            .child("Modified"),
                    ),
            )
            .child(
                div().flex_1().min_h_0().px_1().child(
                    v_virtual_list(
                        cx.entity(),
                        "piku-file-list",
                        sizes,
                        move |this, range, _window, cx| {
                            range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&self.scroll),
                ),
            )
    }

    /// Git status cell for one row: a status letter for files, a dot for
    /// directories containing dirty entries, nothing outside a repository.
    /// Data comes pre-sanitized from the `GitStore`.
    pub(super) fn git_badge(
        &self,
        entry: &FsEntry,
        cx: &Context<Self>,
    ) -> Option<gpui::AnyElement> {
        use gpui::IntoElement as _;
        let git = crate::state::PikuState::global(cx).git.read(cx);
        let cwd = &self.session.cwd;
        git.root_for(cwd)?;
        // Ignored beats everything else (an ignored dir is never "dirty").
        if git.is_ignored(cwd, &entry.path) {
            let ignored = crate::services::git::types::GitFileStatus {
                index: None,
                worktree: Some(crate::services::git::types::GitStatusCode::Ignored),
            };
            return crate::ui::components::status_glyph(ignored, cx).map(|g| g.into_any_element());
        }
        if entry.is_dir() {
            git.dir_dirty(cwd, &entry.path)
                .then(|| crate::ui::components::dirty_dir_dot(cx).into_any_element())
        } else {
            let status = git.status_of(cwd, &entry.path)?;
            crate::ui::components::status_glyph(status, cx).map(|g| g.into_any_element())
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(entry) = self.entries.get(ix) else {
            return div().into_any_element();
        };
        let entry: FsEntry = entry.clone();
        let selected = self.selected.contains(&ix);
        let dimmed = entry.hidden;
        let zoom = self.zoom();
        // In-place rename: this row's label becomes an editor and the row's
        // own mouse handling is suspended until the edit ends.
        let renaming = self.renaming_ix() == Some(ix);
        let editor = if renaming {
            self.inline_edit.as_ref().map(|edit| edit.input.clone())
        } else {
            None
        };

        let size_text = if entry.is_dir() {
            "—".to_string()
        } else {
            format_size(entry.size)
        };

        // Drag payload: the whole selection when this row is part of it, else
        // just this row. A dir row is also a drop target (move/copy into it).
        let drag_paths = self.drag_paths(ix);
        let drag_label: gpui::SharedString = if drag_paths.len() > 1 {
            format!("{} items", drag_paths.len()).into()
        } else {
            entry.name.clone().into()
        };
        let source_id = self.session_id();
        let is_dir = entry.is_dir();
        let dir_dest = entry.path.clone();

        h_flex()
            .id(ix)
            .w_full()
            .h(px(row_height(zoom)))
            .px_2()
            .gap_2()
            .items_center()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            // While renaming, the row's active border/bg are suppressed so the
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
            .when(!renaming, |row| {
                row.on_mouse_down(
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
                .when(!drag_paths.is_empty(), |row| {
                    row.on_drag(
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
                // Only folders accept a drop (into that folder). Ctrl forces a
                // copy; otherwise it's a move. External OS files always copy.
                .when(is_dir, |row| {
                    row.drag_over::<DraggedPaths>(|style, _, _, cx| {
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
            .child(entry_visual(&entry, 16. * zoom, cx))
            .child(match editor {
                Some(input) => div()
                    .flex_1()
                    .min_w_0()
                    .rounded(cx.theme().radius)
                    .on_key_down(cx.listener(ExplorerPanel::on_editor_key_down))
                    .child(gpui_component::input::Input::new(&input).small())
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13. * zoom))
                    .text_color(if dimmed {
                        cx.theme().muted_foreground
                    } else {
                        cx.theme().foreground
                    })
                    .child(entry.name.clone())
                    .into_any_element(),
            })
            .children(self.git_badge(&entry, cx))
            .child(
                div()
                    .w(px(BASE_SIZE_COL * zoom))
                    .flex_none()
                    .text_right()
                    .text_size(px(12. * zoom))
                    .text_color(cx.theme().muted_foreground)
                    .child(size_text),
            )
            .child(
                div()
                    .w(px(BASE_DATE_COL * zoom))
                    .flex_none()
                    .text_right()
                    .text_size(px(12. * zoom))
                    .text_color(cx.theme().muted_foreground)
                    .child(format_time(entry.modified)),
            )
            .into_any_element()
    }
}
