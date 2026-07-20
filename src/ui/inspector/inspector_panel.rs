//! Right-dock inspector: adapts to the current selection with previews and
//! metadata instead of duplicating the file listing.

use std::path::PathBuf;

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    StyledImage as _, Subscription, Window, div, img, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName,
    dock::{Panel, PanelControl, PanelEvent},
    v_flex,
};

use crate::core::entry::FsEntry;
use crate::core::file_type::{categorize, is_image_previewable, is_text_previewable};
use crate::core::format::{format_size, format_time};
use crate::state::PikuState;
use crate::ui::components::{category_icon, empty_state};

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    preview_text: Option<SharedString>,
    preview_for: Option<PathBuf>,
    _subscriptions: Vec<Subscription>,
}

impl InspectorPanel {
    pub const PANEL_NAME: &'static str = "PikuInspector";

    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let selection = PikuState::global(cx).selection.clone();
        let subscription = cx.observe(&selection, |this: &mut Self, selection, cx| {
            this.on_selection_changed(&selection.read(cx).entries.clone(), cx);
            cx.notify();
        });

        Self {
            focus_handle: cx.focus_handle(),
            preview_text: None,
            preview_for: None,
            _subscriptions: vec![subscription],
        }
    }

    fn on_selection_changed(&mut self, entries: &[FsEntry], cx: &mut Context<Self>) {
        let single_text = (entries.len() == 1 && is_text_previewable(&entries[0]))
            .then(|| entries[0].path.clone());

        match single_text {
            None => {
                self.preview_text = None;
                self.preview_for = None;
            }
            Some(path) => {
                if self.preview_for.as_ref() == Some(&path) {
                    return;
                }
                self.preview_for = Some(path.clone());
                self.preview_text = None;
                let load_path = path.clone();
                let task = cx.background_executor().spawn(async move {
                    use std::io::Read as _;
                    // Authorize the path and read at most 4 KiB — never pull a
                    // multi-GB file into memory to preview its head.
                    let load_path = crate::storage::local().guard().sanitize(&load_path).ok()?;
                    let file = std::fs::File::open(&load_path).ok()?;
                    let mut bytes = Vec::with_capacity(4096);
                    file.take(4096).read_to_end(&mut bytes).ok()?;
                    Some(String::from_utf8_lossy(&bytes).into_owned())
                });
                cx.spawn(async move |this, cx| {
                    let text = task.await;
                    let _ = this.update(cx, |this, cx| {
                        if this.preview_for.as_ref() == Some(&path) {
                            this.preview_text = text.map(SharedString::from);
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }

    fn detail_row(label: &'static str, value: impl Into<SharedString>, cx: &App) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(value.into()),
            )
    }

    fn render_single(&self, entry: &FsEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let category = categorize(entry);
        let name: SharedString = entry.name.clone().into();
        let path_text: SharedString = entry.path.display().to_string().into();

        let preview: gpui::AnyElement = if is_image_previewable(entry) {
            div()
                .w_full()
                .h(px(170.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .child(
                    img(entry.path.clone())
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain)
                        .rounded(cx.theme().radius),
                )
                .into_any_element()
        } else if let Some(text) = self.preview_text.clone() {
            div()
                .w_full()
                .h(px(170.))
                .p_2()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .font_family("monospace")
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .overflow_hidden()
                .child(text)
                .into_any_element()
        } else if self.preview_for.as_ref() == Some(&entry.path) {
            // Text preview still loading — same box the preview will occupy,
            // so nothing jumps when the content arrives.
            div()
                .w_full()
                .h(px(170.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .child(crate::ui::components::piku_spinner(
                    gpui_component::Size::Small,
                    cx,
                ))
                .into_any_element()
        } else {
            div()
                .w_full()
                .h(px(120.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .child(
                    category_icon(category)
                        .size(px(52.))
                        .text_color(cx.theme().muted_foreground),
                )
                .into_any_element()
        };

        let mut details = v_flex()
            .gap_3()
            .child(Self::detail_row("Type", category.label(), cx));
        if !entry.is_dir() {
            details = details.child(Self::detail_row("Size", format_size(entry.size), cx));
        }
        details = details
            .child(Self::detail_row("Modified", format_time(entry.modified), cx))
            .child(Self::detail_row("Created", format_time(entry.created), cx))
            .child(Self::detail_row("Location", path_text.clone(), cx));

        let attributes = [
            entry.hidden.then_some("Hidden"),
            entry.readonly.then_some("Read-only"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        if !attributes.is_empty() {
            details = details.child(Self::detail_row("Attributes", attributes, cx));
        }

        v_flex()
            .size_full()
            .gap_3()
            .p_3()
            .child(preview)
            .child(
                div()
                    .text_base()
                    .text_color(cx.theme().foreground)
                    .child(name),
            )
            .child(details)
            .into_any_element()
    }

    fn render_multi(&self, entries: &[FsEntry], cx: &mut Context<Self>) -> gpui::AnyElement {
        let folders = entries.iter().filter(|e| e.is_dir()).count();
        let files = entries.len() - folders;
        let total: u64 = entries.iter().map(|e| e.size).sum();

        v_flex()
            .size_full()
            .gap_3()
            .p_3()
            .child(
                div()
                    .w_full()
                    .h(px(120.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(
                        Icon::new(IconName::Copy)
                            .size(px(44.))
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                div()
                    .text_base()
                    .text_color(cx.theme().foreground)
                    .child(format!("{} items selected", entries.len())),
            )
            .child(
                v_flex()
                    .gap_3()
                    .child(Self::detail_row(
                        "Contents",
                        format!("{folders} folders · {files} files"),
                        cx,
                    ))
                    .child(Self::detail_row("Total file size", format_size(total), cx)),
            )
            .into_any_element()
    }

    fn render_dir_summary(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selection = PikuState::global(cx).selection.read(cx);
        let Some(dir) = selection.dir.clone() else {
            return empty_state(
                Icon::new(IconName::Info),
                "Nothing selected",
                "Select a file to see its details",
                cx,
            )
            .into_any_element();
        };
        let items = selection.dir_items;
        let name: SharedString = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.display().to_string())
            .into();

        v_flex()
            .size_full()
            .gap_3()
            .p_3()
            .child(
                div()
                    .w_full()
                    .h(px(120.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(
                        Icon::new(IconName::FolderOpen)
                            .size(px(52.))
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            .child(
                div()
                    .text_base()
                    .text_color(cx.theme().foreground)
                    .child(name),
            )
            .child(
                v_flex()
                    .gap_3()
                    .child(Self::detail_row("Type", "Folder", cx))
                    .child(Self::detail_row("Items", format!("{items} visible"), cx))
                    .child(Self::detail_row("Location", dir.display().to_string(), cx)),
            )
            .into_any_element()
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entries = PikuState::global(cx).selection.read(cx).entries.clone();

        let body = match entries.len() {
            0 => self.render_dir_summary(cx),
            1 => self.render_single(&entries[0], cx),
            _ => self.render_multi(&entries, cx),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(
                div()
                    .id("inspector-scroll")
                    .size_full()
                    .overflow_y_scroll()
                    .child(body),
            )
    }
}

impl Focusable for InspectorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for InspectorPanel {}

impl Panel for InspectorPanel {
    fn panel_name(&self) -> &'static str {
        Self::PANEL_NAME
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Details"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> Option<PanelControl> {
        None
    }
}
