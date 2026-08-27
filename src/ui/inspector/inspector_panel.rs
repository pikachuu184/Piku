//! Right-dock inspector: adapts to the current selection with a
//! provider-driven preview (see [`crate::preview`]) plus metadata rows.
//!
//! Decoding happens in the backend's preview service. The panel holds the
//! request's `Inflight`; assigning a new one drops the old, which **stops** the
//! superseded decode rather than merely discarding its answer. That is the
//! difference from the generation counter this replaced — holding Down through
//! a folder of videos used to run one ffmpeg subprocess per file to
//! completion.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::SystemTime;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Bounds, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement as _,
    Styled, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    dock::{Panel, PanelControl, PanelEvent},
    input::InputState,
    tab::{Tab, TabBar},
    v_flex,
};

use crate::backend::dispatch::BackendExt as _;
use crate::backend::error::BackendError;
use crate::backend::protocol::Inflight;
use crate::backend::services::preview::PreviewRequest;
use crate::core::entry::{EntryKind, FsEntry};
use crate::core::file_type::categorize;
use crate::core::format::{format_size, format_time};
use crate::preview::content::PreviewContent;
use crate::services::preview_cache::PreviewKey;
use crate::state::PikuState;
use crate::ui::components::{category_icon, empty_state};

pub struct InspectorPanel {
    focus_handle: FocusHandle,
    /// The finished preview for exactly one file, keyed by (path, mtime).
    pub(super) loaded: Option<LoadedPreview>,
    /// Path currently being decoded on the background executor.
    pub(super) loading_for: Option<PathBuf>,
    /// The live preview request. Dropping it cancels the worker; assigning a
    /// new one supersedes the old.
    preview_req: Option<Inflight>,
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
    /// The parsed markdown document, held as an entity rather than rebuilt.
    ///
    /// This is the difference between a document that scrolls and one that
    /// does not. Handing `TextView` a state entity lets it virtualize — only
    /// the visible blocks are laid out — where the by-value constructor lays
    /// out *every* block of a document up to `MARKDOWN_CAP` (512 KiB) on every
    /// single frame. Same shape as [`Self::code_state`] and [`Self::tree_state`].
    pub(super) md_state: Option<Entity<gpui_component::text::TextViewState>>,
    /// Which file the markdown state holds, to avoid re-setting it on render.
    pub(super) md_synced: Option<PathBuf>,
    /// Scroll position of the zoomed image, so zoom can keep the point under
    /// the cursor fixed instead of drifting.
    pub(super) pan_scroll: gpui::ScrollHandle,
    /// Where the current pan drag was last seen. `MouseMoveEvent` carries no
    /// delta of its own, so the difference has to be kept here.
    pub(super) drag_from: Option<gpui::Point<Pixels>>,
    /// Which tab the inspector is showing. Deliberately **not** part of
    /// [`PreviewViewState`]: that is reset on every new file, and per-file zoom
    /// should reset while the tab the user chose should not.
    pub(super) tab: InspectorTab,
    /// The Preview tab's content box, measured during paint.
    ///
    /// This is what replaced deriving a height from `window.viewport_size()`.
    /// The window is the wrong thing to measure: the inspector is a dock panel
    /// whose height also depends on the media bar (zero-height when idle,
    /// ~46 px when playing), the tab bar, and the status bar. Same
    /// `Rc<Cell<Bounds>>` + `canvas` trick the media scrubber uses.
    pub(super) preview_viewport: Rc<Cell<Bounds<Pixels>>>,
    /// State backing the Git mode (commits, expanded detail, file history).
    pub(super) git: super::git_view::GitViewState,
    _subscriptions: Vec<Subscription>,
}

/// The inspector's tabs. One shell, three bodies — the header, tab bar,
/// spacing and scrolling stay identical whichever is active, which is what
/// keeps selecting a different file from feeling like a different dialog.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum InspectorTab {
    /// The file's content, filling the panel edge to edge.
    #[default]
    Preview,
    /// Metadata rows, in their own scroll column.
    Details,
    /// Repository state. Only offered inside a repository.
    Git,
}

impl InspectorTab {
    fn label(self) -> &'static str {
        match self {
            Self::Preview => "Preview",
            Self::Details => "Details",
            Self::Git => "Git",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Preview => Icon::new(crate::app::assets::PikuIcon::Eye),
            Self::Details => Icon::new(IconName::Info),
            Self::Git => Icon::new(crate::app::assets::PikuIcon::GitBranch),
        }
    }
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
    pub image_zoom: ImageZoom,
}

impl Default for PreviewViewState {
    fn default() -> Self {
        Self {
            markdown_raw: false,
            json_pretty: true,
            structured_tree: false,
            image_zoom: ImageZoom::default(),
        }
    }
}

/// How the image preview is scaled.
///
/// This replaced a `bool` plus an `f32`, which could express states that do not
/// exist (fitted *and* at 250 %) and could not express the one that matters:
/// what scale "fit" actually resolved to. `Fit` is still rendered by the
/// renderer's own `ObjectFit::Contain` rather than by a number we compute —
/// that needs no measurement, so it cannot flash on the first frame or lag a
/// panel resize. The measured viewport is used only to *report* the fitted
/// percentage and to seed [`ImageZoom::Custom`] when the user zooms out of
/// `Fit`, so zooming in continues from what is on screen instead of jumping
/// to 100 %.
#[derive(Clone, Copy, PartialEq, Default)]
pub(super) enum ImageZoom {
    #[default]
    Fit,
    /// One source pixel per display unit.
    Actual,
    Custom(f32),
}

/// Zoom bounds. Unchanged from the pair of fields this replaced.
pub(super) const ZOOM_MIN: f32 = 0.1;
pub(super) const ZOOM_MAX: f32 = 8.0;

impl ImageZoom {
    /// The scale this state resolves to, given the fitted scale.
    ///
    /// `fit_scale` is `None` before the viewport has been measured, or when the
    /// image has no pixel dimensions (SVG) — in which case there is no number
    /// to report and the caller falls back to `Contain`.
    pub fn scale(self, fit_scale: Option<f32>) -> Option<f32> {
        match self {
            Self::Fit => fit_scale,
            Self::Actual => Some(1.0),
            Self::Custom(scale) => Some(scale),
        }
    }

    pub fn is_fit(self) -> bool {
        matches!(self, Self::Fit)
    }

    /// Multiply the current scale, resolving `Fit` against the measured
    /// viewport first so the step continues from what is on screen.
    pub fn stepped(self, factor: f32, fit_scale: Option<f32>) -> Self {
        let base = self.scale(fit_scale).unwrap_or(1.0);
        Self::Custom((base * factor).clamp(ZOOM_MIN, ZOOM_MAX))
    }
}

/// The scale that fits `(width, height)` inside `viewport`.
///
/// Mirrors the arithmetic Zed's own image viewer uses
/// (`compute_fit_to_view_zoom`), without its `.min(1.0)`: PIKU's fitted mode
/// has always upscaled a small image to fill the box, and this is a report of
/// that behaviour, not a change to it.
pub(super) fn fit_scale(viewport: Bounds<Pixels>, dimensions: (u32, u32)) -> Option<f32> {
    let (width, height) = dimensions;
    if width == 0 || height == 0 {
        return None;
    }
    let vw = f32::from(viewport.size.width);
    let vh = f32::from(viewport.size.height);
    if vw <= 0.0 || vh <= 0.0 {
        return None;
    }
    Some((vw / width as f32).min(vh / height as f32))
}

impl InspectorPanel {
    /// Step the zoom while keeping the image point under `cursor` in place.
    ///
    /// Without this the image appears to slide out from under the pointer as
    /// you zoom, because the scroll container keeps its old offset while the
    /// content grows around it. The correction is to find which point of the
    /// image the cursor is over, then choose the offset that puts that same
    /// point back under the cursor at the new scale:
    ///
    /// ```text
    /// point      = (offset + cursor_local) / old_scale
    /// new_offset = point * new_scale - cursor_local
    /// ```
    ///
    /// GPUI's scroll offsets run negative as content moves up and left, so the
    /// stored offset is negated on the way in and back out.
    pub(super) fn zoom_about(
        &mut self,
        cursor: gpui::Point<Pixels>,
        factor: f32,
        fitted: Option<f32>,
        dimensions: Option<(u32, u32)>,
    ) {
        let before = self.view.image_zoom.scale(fitted).unwrap_or(1.0);
        self.view.image_zoom = self.view.image_zoom.stepped(factor, fitted);
        let after = self.view.image_zoom.scale(fitted).unwrap_or(1.0);

        // Fitted mode centres the image itself, so there is no offset to keep.
        let Some(dimensions) = dimensions else { return };
        let (before, after) = (
            super::preview_view::effective_scale(before, dimensions),
            super::preview_view::effective_scale(after, dimensions),
        );
        if before <= 0.0 || after <= 0.0 || before == after {
            return;
        }

        let viewport = self.preview_viewport.get();
        let local = cursor - viewport.origin;
        let offset = self.pan_scroll.offset();
        let point_x = (f32::from(local.x) - f32::from(offset.x)) / before;
        let point_y = (f32::from(local.y) - f32::from(offset.y)) / before;
        self.pan_scroll.set_offset(gpui::point(
            px(f32::from(local.x) - point_x * after),
            px(f32::from(local.y) - point_y * after),
        ));
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
            preview_req: None,
            view: PreviewViewState::default(),
            code_state: None,
            code_synced: None,
            tree_state: None,
            tree_synced: None,
            md_state: None,
            md_synced: None,
            pan_scroll: gpui::ScrollHandle::new(),
            drag_from: None,
            tab: InspectorTab::default(),
            preview_viewport: Rc::new(Cell::new(Bounds::default())),
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

        self.view = PreviewViewState::default();
        // Drop the previous document rather than re-pointing it: `TextViewState`
        // owns a scroll position as well as the parsed blocks, and inheriting
        // the last file's scroll offset would open the next one part-way down.
        self.md_state = None;
        self.md_synced = None;
        self.pan_scroll = gpui::ScrollHandle::new();
        self.drag_from = None;

        let path = entry.path.clone();
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
            // Nothing in flight is worth finishing now.
            self.preview_req = None;
            return;
        }

        self.loading_for = Some(entry.path.clone());
        let request = PreviewRequest::for_entry(&entry);
        // Assigning drops the previous Inflight, which cancels the worker that
        // was decoding whatever was selected a moment ago.
        self.preview_req = Some(cx.backend_task_cancellable(
            move |backend| backend.preview().preview(request),
            move |this: &mut Self, result, cx| {
                this.preview_req = None;
                let content = match result {
                    Ok(Ok(ready)) => {
                        // Cache under the key the worker actually read, not the
                        // one guessed from a possibly-stale directory listing.
                        let content = Arc::new(PreviewContent::from(ready.payload));
                        let cache = PikuState::global(cx).preview_cache.clone();
                        // Not a bare `insert`: whatever this displaces still owns
                        // a sprite-atlas tile, and only `drop_image` frees it.
                        crate::services::preview_cache::cache_preview(
                            &cache,
                            ready.key,
                            content.clone(),
                            cx,
                        );
                        content
                    }
                    // Superseded or shutting down: the newer request owns the
                    // panel now, so leave its loading state alone.
                    Ok(Err(error)) if error.is_cancelled() => return,
                    Err(error) if error.is_cancelled() => return,
                    // `user_message` rather than `to_string`: these embed
                    // filenames, and a filename is attacker-controlled.
                    Ok(Err(error)) => Arc::new(PreviewContent::Error(
                        BackendError::from(error).user_message().into(),
                    )),
                    Err(error) => Arc::new(PreviewContent::Error(error.user_message().into())),
                };
                this.loaded = Some(LoadedPreview {
                    path,
                    mtime,
                    content,
                });
                this.loading_for = None;
            },
        ));
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

    /// The Preview tab's body for a single selected file: the decoded content,
    /// a spinner while it is decoding, or the file's category glyph.
    ///
    /// Every branch is `size_full()`. The caller has already given this a real
    /// bounded box (see `render`), so "full" means the panel — which is the
    /// whole point of the tab: the preview gets the height and the width, and
    /// the metadata lives in its own tab rather than crowding it.
    fn render_preview_tab(
        &mut self,
        entry: &FsEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let has_loaded = entry.kind == EntryKind::File
            && self.loaded.as_ref().is_some_and(|l| l.path == entry.path);
        let is_loading =
            entry.kind == EntryKind::File && self.loading_for.as_ref() == Some(&entry.path);

        if has_loaded {
            return super::preview_view::render_preview_box(self, window, cx);
        }
        let filler = if is_loading {
            crate::ui::components::piku_spinner(gpui_component::Size::Small, cx).into_any_element()
        } else {
            category_icon(categorize(entry))
                .size(px(52.))
                .text_color(cx.theme().muted_foreground)
                .into_any_element()
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().muted)
            .child(filler)
            .into_any_element()
    }

    fn render_single(&mut self, entry: &FsEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let category = categorize(entry);
        let path_text: SharedString = entry.path.display().to_string().into();

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
        // be pinned to the scroll viewport height, clipping tall content
        // instead of letting the enclosing `overflow_y_scroll` reach the end of
        // it. The preview no longer lives here, so it is no longer constrained
        // by this: it has its own tab, with a real bounded box.
        v_flex().w_full().gap_3().child(details).into_any_element()
    }

    /// A glyph filling the Preview tab. Used for every selection that has no
    /// decodable content — a folder, a multi-selection, nothing at all — so the
    /// tab is never empty and the shell never changes shape.
    fn render_glyph_tab(icon: Icon, cx: &Context<Self>) -> gpui::AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().muted)
            .child(icon.size(px(52.)).text_color(cx.theme().muted_foreground))
            .into_any_element()
    }

    fn render_multi(&self, entries: &[FsEntry], cx: &mut Context<Self>) -> gpui::AnyElement {
        let folders = entries.iter().filter(|e| e.is_dir()).count();
        let files = entries.len() - folders;
        let total: u64 = entries.iter().map(|e| e.size).sum();

        v_flex()
            .w_full()
            .gap_3()
            .child(Self::detail_row(
                "Contents",
                format!("{folders} folders · {files} files"),
                cx,
            ))
            .child(Self::detail_row("Total file size", format_size(total), cx))
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

        v_flex()
            .w_full()
            .gap_3()
            .child(Self::detail_row("Type", "Folder", cx))
            .child(Self::detail_row("Items", format!("{items} visible"), cx))
            .child(Self::detail_row("Location", dir.display().to_string(), cx))
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

    /// The shell's header: what is selected, stated once.
    ///
    /// It sits above the tab bar and therefore does not change when the tab
    /// does — so the name and path stay visible while the Preview tab runs
    /// edge-to-edge with no room for a caption of its own.
    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selection = PikuState::global(cx).selection.read(cx);
        let entries = &selection.entries;

        let (icon, title, subtitle): (Icon, SharedString, Option<SharedString>) =
            match entries.len() {
                1 => {
                    let entry = &entries[0];
                    (
                        category_icon(categorize(entry)),
                        entry.name.clone().into(),
                        Some(entry.path.display().to_string().into()),
                    )
                }
                0 => match selection.dir.as_deref() {
                    Some(dir) => (
                        Icon::new(IconName::FolderOpen),
                        dir.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| dir.display().to_string())
                            .into(),
                        Some(dir.display().to_string().into()),
                    ),
                    None => (Icon::new(IconName::Info), "Nothing selected".into(), None),
                },
                count => (
                    Icon::new(IconName::Copy),
                    format!("{count} items selected").into(),
                    None,
                ),
            };

        gpui_component::h_flex()
            .flex_none()
            .w_full()
            .gap_2()
            .px_3()
            .pt_2()
            .items_center()
            .child(icon.size(px(15.)).text_color(cx.theme().muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(title),
                    )
                    .when_some(subtitle, |column, subtitle| {
                        column.child(
                            div()
                                .w_full()
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(subtitle),
                        )
                    }),
            )
    }

    /// Preview ⇄ Details ⇄ Git, with the active preview's own controls on the
    /// right of the same row.
    ///
    /// Git only appears inside a repository; the other two are always present,
    /// so the bar never changes shape as you move between files. The controls
    /// go in `TabBar`'s suffix, which renders after the tab list with the empty
    /// space between — so they sit right-aligned, in one row, wherever they
    /// came from. That is why no preview block draws a control strip of its own
    /// any more.
    fn render_tab_bar(&self, in_repo: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let order = self.tab_order(in_repo);
        let selected = order.iter().position(|t| *t == self.tab).unwrap_or(0);
        let actions = super::preview_view::preview_actions(self, cx);

        let mut bar = TabBar::new("inspector-tabs")
            .segmented()
            .small()
            .selected_index(selected)
            .on_click({
                let order = order.clone();
                cx.listener(move |this, index: &usize, _, cx| {
                    if let Some(tab) = order.get(*index) {
                        this.tab = *tab;
                        cx.notify();
                    }
                })
            });
        for tab in &order {
            // The icon goes in `prefix`, not `icon`: `Tab` renders icon *or*
            // label, never both, so `.icon()` would silently drop the name and
            // leave three unlabelled glyphs. `prefix` renders beside the label.
            bar = bar.child(
                Tab::new()
                    .prefix(
                        tab.icon()
                            .size(px(12.))
                            .text_color(cx.theme().muted_foreground),
                    )
                    .label(tab.label()),
            );
        }

        div()
            .flex_none()
            .px_3()
            .pb_2()
            .child(bar.when_some(actions, |bar, actions| bar.suffix(actions)))
    }

    /// The tabs on offer, in bar order. One list so the rendered order and the
    /// index the click handler resolves cannot disagree.
    fn tab_order(&self, in_repo: bool) -> Vec<InspectorTab> {
        let mut order = vec![InspectorTab::Preview, InspectorTab::Details];
        if in_repo {
            order.push(InspectorTab::Git);
        }
        order
    }
}

impl Render for InspectorPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("InspectorPanel");
        let entries = PikuState::global(cx).selection.read(cx).entries.clone();
        let in_repo = self.repo_root(cx).is_some();
        // Leaving every repository closes the Git tab; falling back to Details
        // rather than to Preview keeps the user on metadata, which is what they
        // were looking at.
        if !in_repo && self.tab == InspectorTab::Git {
            self.tab = InspectorTab::Details;
        }

        // The body is `flex_1().min_h_0()` and, crucially, the Preview tab
        // inside it is NOT wrapped in a scroll container. That is what makes
        // `size_full()` in a preview block mean *the panel*: inside a scroll
        // container, height resolves against content, so the previous version
        // had to guess a height from `window.viewport_size()`. The dock already
        // hands this panel a real bounded box (`shell.rs` mounts the dock area
        // as `flex_1().min_h_0()`), so the height was always there to take.
        let body = match self.tab {
            InspectorTab::Preview => {
                let store = self.preview_viewport.clone();
                let content: gpui::AnyElement = match entries.len() {
                    0 => match PikuState::global(cx).selection.read(cx).dir.is_some() {
                        true => Self::render_glyph_tab(Icon::new(IconName::FolderOpen), cx),
                        false => empty_state(
                            Icon::new(IconName::Info),
                            "Nothing selected",
                            "Select a file to preview it",
                            cx,
                        )
                        .into_any_element(),
                    },
                    1 => self.render_preview_tab(&entries[0], window, cx),
                    _ => Self::render_glyph_tab(Icon::new(IconName::Copy), cx),
                };
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    // Measures the tab's content box. `absolute().inset_0()` so
                    // it spans the container without taking part in its layout
                    // — a zero-sized canvas would measure nothing.
                    //
                    // Nothing *sizes* off this: fitted mode is still
                    // `ObjectFit::Contain`, so there is no first frame to
                    // flash. It feeds the zoom readout and the seed for leaving
                    // fit, both of which are read on the following render — so
                    // ask for one when, and only when, the box actually
                    // changed. Guarding on the change is what keeps this from
                    // being a render loop.
                    .child(
                        gpui::canvas(
                            move |bounds, window, _| {
                                if store.get() != bounds {
                                    store.set(bounds);
                                    window.refresh();
                                }
                            },
                            |_, _, _, _| (),
                        )
                        .absolute()
                        .inset_0(),
                    )
                    .child(content)
                    .into_any_element()
            }
            InspectorTab::Details => {
                let details = match entries.len() {
                    0 => self.render_dir_summary(cx),
                    1 => self.render_single(&entries[0], cx),
                    _ => self.render_multi(&entries, cx),
                };
                div()
                    .id("inspector-details")
                    .size_full()
                    .overflow_y_scroll()
                    .p_3()
                    .child(details)
                    .into_any_element()
            }
            InspectorTab::Git => div()
                .id("inspector-git")
                .size_full()
                .overflow_y_scroll()
                .child(super::git_view::render_git(self, window, cx))
                .into_any_element(),
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(self.render_header(cx))
            .child(self.render_tab_bar(in_repo, cx))
            .child(div().flex_1().min_h_0().child(body))
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
