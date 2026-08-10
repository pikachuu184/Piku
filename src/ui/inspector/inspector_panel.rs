//! Right-dock inspector: adapts to the current selection with a
//! provider-driven preview (see [`crate::preview`]) plus metadata rows.
//! Decoding always happens on the background executor; a generation counter
//! plus a (path, mtime) key guard against stale results and double loads.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName,
    dock::{Panel, PanelControl, PanelEvent},
    input::InputState,
    v_flex,
};

use crate::core::entry::{EntryKind, FsEntry};
use crate::core::file_type::categorize;
use crate::core::format::{format_size, format_time};
use crate::preview::content::PreviewContent;
use crate::preview::{decide_kind, load_preview};
use crate::services::preview_cache::PreviewKey;
use crate::state::PikuState;
use crate::ui::components::{category_icon, empty_state};

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    /// The finished preview for exactly one file, keyed by (path, mtime).
    pub(super) loaded: Option<LoadedPreview>,
    /// Path currently being decoded on the background executor.
    pub(super) loading_for: Option<PathBuf>,
    /// Staleness guard: results from an older generation are dropped.
    generation: u64,
    /// Ephemeral per-file view toggles (reset on every new file).
    pub(super) view: PreviewViewState,
    /// One lazily-created code editor entity, re-pointed per file.
    pub(super) code_state: Option<Entity<InputState>>,
    /// What the code editor currently holds, to avoid re-setting on render.
    pub(super) code_synced: Option<CodeSyncKey>,
    /// Lazily-created tree for the structured (JSON/YAML/TOML) tree view.
    pub(super) tree_state: Option<Entity<gpui_component::tree::TreeState>>,
    /// Which file the tree currently holds, to avoid rebuilding on every render.
    pub(super) tree_synced: Option<PathBuf>,
    /// Details vs. Git mode (ephemeral, only offered inside a repository).
    pub(super) mode: InspectorMode,
    /// State backing the Git mode (commits, expanded detail, file history).
    pub(super) git: super::git_view::GitViewState,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum InspectorMode {
    #[default]
    Details,
    Git,
}

pub(super) struct LoadedPreview {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    /// `Arc` so a cache hit hands over the shared decoded payload without a copy.
    pub content: Arc<PreviewContent>,
}

pub(super) struct PreviewViewState {
    pub markdown_raw: bool,
    pub json_pretty: bool,
    /// Structured data: show the collapsible tree instead of pretty/raw text.
    pub structured_tree: bool,
    pub image_fit: bool,
    pub image_zoom: f32,
}

impl Default for PreviewViewState {
    fn default() -> Self {
        Self {
            markdown_raw: false,
            json_pretty: true,
            structured_tree: false,
            image_fit: true,
            image_zoom: 1.0,
        }
    }
}

/// Identity of the text the shared code editor was last synced with.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct CodeSyncKey {
    pub path: PathBuf,
    pub variant: &'static str,
}

impl InspectorPanel {
    pub const PANEL_NAME: &'static str = "PikuInspector";

    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        let selection = PikuState::global(cx).selection.clone();
        let subscription = cx.observe(&selection, |this: &mut Self, selection, cx| {
            this.on_selection_changed(&selection.read(cx).entries.clone(), cx);
            cx.notify();
        });

        // Re-render as audio plays so the transport bar / waveform advance.
        let audio = PikuState::global(cx).audio.clone();
        let audio_sub = cx.observe(&audio, |_, _, cx| cx.notify());

        // Re-render when repository state changes (branch, status counts).
        let git = PikuState::global(cx).git.clone();
        let git_sub = cx.observe(&git, |_, _, cx| cx.notify());

        Self {
            focus_handle: cx.focus_handle(),
            loaded: None,
            loading_for: None,
            generation: 0,
            view: PreviewViewState::default(),
            code_state: None,
            code_synced: None,
            tree_state: None,
            tree_synced: None,
            mode: InspectorMode::default(),
            git: super::git_view::GitViewState::default(),
            _subscriptions: vec![subscription, audio_sub, git_sub],
        }
    }

    fn on_selection_changed(&mut self, entries: &[FsEntry], cx: &mut Context<Self>) {
        let single_file =
            (entries.len() == 1 && entries[0].kind == EntryKind::File).then(|| entries[0].clone());
        let Some(entry) = single_file else {
            self.loaded = None;
            self.loading_for = None;
            return;
        };

        // Minimal cache: skip the reload when the same unchanged file is
        // re-selected; a changed mtime forces a fresh decode.
        if self
            .loaded
            .as_ref()
            .is_some_and(|l| l.path == entry.path && l.mtime == entry.modified)
        {
            return;
        }
        if self.loading_for.as_ref() == Some(&entry.path) {
            return;
        }

        self.generation += 1;
        let generation = self.generation;
        self.view = PreviewViewState::default();

        let kind = decide_kind(&entry);
        let path = entry.path.clone();
        let ext = entry.ext.clone();
        let mtime = entry.modified;
        let key = PreviewKey::new(path.clone(), mtime, entry.size);

        // Cache hit: the decoded preview for this exact (path, mtime, size) is
        // already resident — hand it over immediately, no decode, no spinner.
        let cache = PikuState::global(cx).preview_cache.clone();
        if let Some(content) = cache.update(cx, |c, _| c.get(&key)) {
            self.loaded = Some(LoadedPreview {
                path,
                mtime,
                content,
            });
            self.loading_for = None;
            return;
        }

        self.loading_for = Some(entry.path.clone());
        let task = cx
            .background_executor()
            .spawn(async move { (load_preview(kind, &path, &ext), path) });
        cx.spawn(async move |this, cx| {
            let (content, path) = task.await;
            let content = Arc::new(content);
            let _ = this.update(cx, |this, cx| {
                // Populate the shared cache regardless of staleness, so the work
                // is not wasted even if the selection moved on mid-decode.
                let cache = PikuState::global(cx).preview_cache.clone();
                cache.update(cx, |c, _| c.insert(key, content.clone()));
                if this.generation == generation {
                    this.loaded = Some(LoadedPreview {
                        path,
                        mtime,
                        content,
                    });
                    this.loading_for = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub(super) fn detail_row(
        label: &'static str,
        value: impl Into<SharedString>,
        cx: &App,
    ) -> impl IntoElement {
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

    fn render_single(
        &mut self,
        entry: &FsEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let category = categorize(entry);
        let name: SharedString = entry.name.clone().into();
        let path_text: SharedString = entry.path.display().to_string().into();

        let has_loaded = entry.kind == EntryKind::File
            && self.loaded.as_ref().is_some_and(|l| l.path == entry.path);
        let is_loading =
            entry.kind == EntryKind::File && self.loading_for.as_ref() == Some(&entry.path);

        let preview: gpui::AnyElement = if has_loaded {
            super::preview_view::render_preview_box(self, window, cx)
        } else if is_loading {
            // Same box the preview will occupy, so nothing jumps when the
            // content arrives.
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
        if let Some(loaded) = &self.loaded
            && loaded.path == entry.path
            && let PreviewContent::Image {
                dimensions: Some((w, h)),
                ..
            } = &*loaded.content
        {
            details = details.child(Self::detail_row("Dimensions", format!("{w} × {h}"), cx));
        }
        details = details
            .child(Self::detail_row(
                "Modified",
                format_time(entry.modified),
                cx,
            ))
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

        // `w_full` (natural height) — NOT `size_full`: an `h_full` body would
        // be pinned to the scroll viewport height, clipping tall content (e.g.
        // rendered markdown) instead of letting the outer `overflow_y_scroll`
        // reach the end of the document.
        v_flex()
            .w_full()
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

impl InspectorPanel {
    /// The repository containing the active pane's directory, when known.
    pub(super) fn repo_root(&self, cx: &App) -> Option<PathBuf> {
        let selection = PikuState::global(cx).selection.read(cx);
        let dir = selection.dir.as_deref()?;
        PikuState::global(cx)
            .git
            .read(cx)
            .root_for(dir)
            .map(|root| root.to_path_buf())
    }

    /// Details ⇄ Git segmented toggle, shown only inside a repository.
    fn render_mode_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let segment =
            |label: &'static str, icon: gpui_component::Icon, mode: InspectorMode, active: bool| {
                gpui_component::h_flex()
                    .id(label)
                    .px_2()
                    .py_0p5()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .when(active, |s| {
                        s.bg(cx.theme().list_active)
                            .text_color(cx.theme().foreground)
                    })
                    .when(!active, |s| {
                        s.text_color(cx.theme().muted_foreground)
                            .hover(|s| s.bg(cx.theme().list_hover))
                    })
                    .child(icon.size(px(12.)))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.mode = mode;
                        cx.notify();
                    }))
            };
        gpui_component::h_flex()
            .gap_1()
            .px_3()
            .pt_2()
            .child(segment(
                "Details",
                Icon::new(IconName::Info),
                InspectorMode::Details,
                self.mode == InspectorMode::Details,
            ))
            .child(segment(
                "Git",
                Icon::new(crate::app::assets::PikuIcon::GitBranch),
                InspectorMode::Git,
                self.mode == InspectorMode::Git,
            ))
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("InspectorPanel");
        let entries = PikuState::global(cx).selection.read(cx).entries.clone();
        let in_repo = self.repo_root(cx).is_some();
        // Leaving every repository resets the toggle; re-entering one starts
        // from Details rather than a stale Git mode.
        if !in_repo {
            self.mode = InspectorMode::Details;
        }

        let body = if in_repo && self.mode == InspectorMode::Git {
            super::git_view::render_git(self, window, cx)
        } else {
            match entries.len() {
                0 => self.render_dir_summary(cx),
                1 => self.render_single(&entries[0], window, cx),
                _ => self.render_multi(&entries, cx),
            }
        };

        let mut root = v_flex().size_full().bg(cx.theme().sidebar);
        if in_repo {
            root = root.child(self.render_mode_toggle(cx));
        }
        root.child(
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
