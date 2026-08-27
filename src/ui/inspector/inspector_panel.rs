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
    IntoElement, ParentElement, Pixels, Render, RenderImage, SharedString,
    StatefulInteractiveElement as _, Styled, Subscription, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    dock::{Panel, PanelControl, PanelEvent, TabPanel},
    input::InputState,
    tab::{Tab, TabBar},
    v_flex,
};

use crate::app::assets::PikuIcon;
use crate::backend::dispatch::BackendExt as _;
use crate::backend::error::BackendError;
use crate::backend::protocol::Inflight;
use crate::backend::services::preview::{FrameSpec, PreviewRequest};
use crate::core::entry::{EntryKind, FsEntry};
use crate::core::file_type::categorize;
use crate::core::format::{format_size, format_time};
use crate::preview::content::PreviewContent;
use crate::security::text::sanitize_path;
use crate::services::preview_cache::PreviewKey;
use crate::state::PikuState;
use crate::ui::components::{category_icon, empty_state};
use crate::ui::inspector::viewport::{ImageZoom, Metrics, Viewport};

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
    /// The one frame derived from the loaded file — page N of a PDF, or the
    /// image turned a quarter of the way round. See [`DerivedFrame`].
    pub(super) derived: Option<DerivedFrame>,
    /// The live derived-frame request. Same discipline as [`Self::preview_req`]:
    /// assigning a new one drops the old, which **stops** the worker rendering
    /// the frame it supersedes. That is what keeps holding the page-forward key
    /// down from rasterizing every page it passes through.
    derived_req: Option<Inflight>,
    /// What [`Self::derived_req`] is fetching, so a second click on the same
    /// button — or a re-render — does not queue the same frame twice.
    derived_pending: Option<(PathBuf, FrameSpec)>,
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
            Self::Preview => Icon::new(PikuIcon::Eye),
            Self::Details => Icon::new(PikuIcon::Info),
            Self::Git => Icon::new(PikuIcon::GitBranch),
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
    /// Zoom and pan for whichever preview has a viewport. Living here rather
    /// than on the panel is what makes it reset with the file: the whole of this
    /// struct is replaced on every new selection.
    pub viewport: Viewport,
    /// Page shown by a paginated preview, zero-based.
    pub page: usize,
    /// Quarter turns clockwise, 0..4.
    ///
    /// Deliberately not part of [`Viewport`], which is pure geometry it can
    /// apply itself. A turn is *new pixels*: gpui's `with_transformation` exists
    /// on `svg` and not on `img`, so there is no way to turn a raster image in
    /// the renderer. What lives here is therefore the request, and the answer
    /// lives in [`InspectorPanel::derived`].
    pub rotation: u8,
}

impl Default for PreviewViewState {
    fn default() -> Self {
        Self {
            markdown_raw: false,
            json_pretty: true,
            structured_tree: false,
            viewport: Viewport::default(),
            page: 0,
            rotation: 0,
        }
    }
}

/// One frame produced from the loaded file *after* its preview arrived.
///
/// Two things the viewer needs do not fit the preview cache's per-file key: page
/// N of a PDF and a turned raster image. Both are the same request shape (see
/// [`FrameSpec`]), so the panel holds exactly one slot for the answer rather
/// than a second keying scheme.
///
/// `path` and `spec` together are the identity the render path checks before
/// drawing this as anything. `dimensions` is stored rather than read back off
/// the `RenderImage` because that is the size the worker actually produced, and
/// it is what the PDF page's viewport metrics are computed from.
pub(super) struct DerivedFrame {
    pub path: PathBuf,
    pub spec: FrameSpec,
    pub dimensions: (u32, u32),
    /// Owns a sprite-atlas tile that only `App::drop_image` frees — which is
    /// why every path out of this slot goes through
    /// [`InspectorPanel::release_derived`] or the explicit release in
    /// [`InspectorPanel::request_frame`].
    pub image: Arc<RenderImage>,
}

/// `dimensions` with the axes swapped for an odd number of quarter turns.
fn oriented(dimensions: (u32, u32), quarter_turns: u8) -> (u32, u32) {
    let (width, height) = dimensions;
    match quarter_turns % 2 {
        1 => (height, width),
        _ => (width, height),
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

        // The derived frame owns a sprite-atlas tile, and a plain `Drop` cannot
        // reach an `App` to free it. `on_release` hands one over at teardown,
        // which is the only leg of the release discipline `release_derived`
        // cannot cover. The panel is `closable() == false`, so in practice this
        // fires at shutdown — but "in practice" is not a guarantee, and the leak
        // it would otherwise cover is invisible.
        let release = cx.on_release(|this: &mut Self, cx| {
            if let Some(frame) = this.derived.take() {
                cx.drop_image(frame.image, None);
            }
        });

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
            tab: InspectorTab::default(),
            preview_viewport: Rc::new(Cell::new(Bounds::default())),
            derived: None,
            derived_req: None,
            derived_pending: None,
            git: super::git_view::GitViewState::default(),
            _subscriptions: vec![subscription, audio_sub, git_sub, release],
        }
    }

    fn on_selection_changed(&mut self, entries: &[FsEntry], cx: &mut Context<Self>) {
        let single_file =
            (entries.len() == 1 && entries[0].kind == EntryKind::File).then(|| entries[0].clone());
        let Some(entry) = single_file else {
            self.loaded = None;
            self.loading_for = None;
            self.release_derived(cx);
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

        // Resets the zoom, pan, rotation and page along with the view toggles:
        // they all belong to the file that was open, not to the panel.
        self.view = PreviewViewState::default();
        // And so does the derived frame. Releasing it here is what keeps paging
        // through a folder of PDFs from climbing: the tile it holds is freed on
        // the way out rather than when the next one happens to displace it.
        self.release_derived(cx);
        // Drop the previous document rather than re-pointing it: `TextViewState`
        // owns a scroll position as well as the parsed blocks, and inheriting
        // the last file's scroll offset would open the next one part-way down.
        self.md_state = None;
        self.md_synced = None;

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

    /// One label-over-value pair. The label takes any string, not only a
    /// `&'static str`, because the media rows come from the decoded preview
    /// and are named by the file rather than by us.
    pub(super) fn detail_row(
        label: impl Into<SharedString>,
        value: impl Into<SharedString>,
        cx: &App,
    ) -> impl IntoElement {
        v_flex()
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.into()),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child(value.into()),
            )
    }

    /// A titled group of rows, or nothing at all when the group is empty.
    ///
    /// Returning `None` is what makes Details contextual rather than a table
    /// of blanks: an image has no Duration, a text file has no Dimensions, and
    /// neither should see the heading. The title is separated from the row
    /// labels by weight and a hairline, not by a new size or colour — both are
    /// `text_xs`, the title in `foreground` and the labels in
    /// `muted_foreground`.
    fn detail_section(
        title: &'static str,
        rows: Vec<(SharedString, SharedString)>,
        cx: &App,
    ) -> Option<impl IntoElement> {
        if rows.is_empty() {
            return None;
        }
        let mut column = v_flex().w_full().gap_3();
        for (label, value) in rows {
            column = column.child(Self::detail_row(label, value, cx));
        }
        Some(
            v_flex()
                .w_full()
                .gap_2()
                .child(
                    div()
                        .w_full()
                        .pb_1()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(title),
                )
                .child(column),
        )
    }

    /// Icon, name and a one-line summary: what the deleted header used to say
    /// above the tab bar, now at the top of the tab that is actually about
    /// identity. The Preview tab is content and nothing else as a result.
    fn identity_block(
        icon: Icon,
        name: impl Into<SharedString>,
        summary: impl Into<SharedString>,
        cx: &App,
    ) -> impl IntoElement {
        gpui_component::h_flex()
            .w_full()
            .gap_3()
            .items_center()
            .child(icon.size(px(28.)).text_color(cx.theme().muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .child(name.into()),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary.into()),
                    ),
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
        if is_loading {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(cx.theme().muted)
                .child(crate::ui::components::piku_spinner(
                    gpui_component::Size::Small,
                    cx,
                ))
                .into_any_element();
        }
        // Nothing decodable. Name it rather than showing a bare glyph: with the
        // header gone this is the only thing in the tab.
        Self::render_glyph_tab(
            category_icon(categorize(entry)),
            entry.name.clone(),
            categorize(entry).label(),
            cx,
        )
    }

    /// Metadata the preview has already decoded.
    ///
    /// Free, not new work: these are the very rows the Preview tab draws for
    /// audio and video, and the counts the PDF and archive blocks show in their
    /// own headers. Surfacing them here costs an iteration over a short Vec.
    fn media_rows(&self, entry: &FsEntry) -> Vec<(SharedString, SharedString)> {
        let Some(loaded) = &self.loaded else {
            return Vec::new();
        };
        if loaded.path != entry.path {
            return Vec::new();
        }
        match &*loaded.content {
            PreviewContent::Image {
                dimensions: Some((width, height)),
                ..
            } => vec![("Dimensions".into(), format!("{width} × {height}").into())],
            PreviewContent::Audio { rows, .. } | PreviewContent::Video { rows, .. } => rows.clone(),
            PreviewContent::Pdf { total_pages, .. } => {
                vec![("Pages".into(), total_pages.to_string().into())]
            }
            PreviewContent::Archive { total_count, .. } => {
                vec![("Entries".into(), total_count.to_string().into())]
            }
            PreviewContent::Hex { signature, .. } => signature
                .map(|signature| vec![("Format".into(), signature.into())])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    fn render_single(&mut self, entry: &FsEntry, cx: &mut Context<Self>) -> gpui::AnyElement {
        let category = categorize(entry);

        let mut general: Vec<(SharedString, SharedString)> =
            vec![("Type".into(), category.label().into())];
        if !entry.is_dir() {
            general.push(("Size".into(), format_size(entry.size).into()));
        }

        let dates: Vec<(SharedString, SharedString)> = vec![
            ("Modified".into(), format_time(entry.modified).into()),
            ("Created".into(), format_time(entry.created).into()),
        ];

        // Sanitized, not `Path::display()`. `entry.name` is cleaned once at
        // construction; the path deliberately keeps its real bytes because it
        // is what gets opened, so it has to be cleaned here instead — and a
        // path is the more exposed of the two, since it carries every ancestor
        // directory's name as well.
        let mut location: Vec<(SharedString, SharedString)> =
            vec![("Path".into(), sanitize_path(&entry.path).into())];
        if let Some(parent) = entry.path.parent() {
            location.push(("Parent folder".into(), sanitize_path(parent).into()));
        }

        let attributes = [
            entry.hidden.then_some("Hidden"),
            entry.readonly.then_some("Read-only"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let permissions: Vec<(SharedString, SharedString)> = if attributes.is_empty() {
            Vec::new()
        } else {
            vec![("Attributes".into(), attributes.into())]
        };

        let summary = match entry.is_dir() {
            true => SharedString::from(category.label()),
            false => format!("{} · {}", category.label(), format_size(entry.size)).into(),
        };

        // `w_full` (natural height) — NOT `size_full`: an `h_full` body would
        // be pinned to the scroll viewport height, clipping tall content
        // instead of letting the enclosing `overflow_y_scroll` reach the end of
        // it. The preview no longer lives here, so it is no longer constrained
        // by this: it has its own tab, with a real bounded box.
        v_flex()
            .w_full()
            .gap_4()
            .child(Self::identity_block(
                category_icon(category),
                entry.name.clone(),
                summary,
                cx,
            ))
            .children(Self::detail_section("General", general, cx))
            .children(Self::detail_section("Media", self.media_rows(entry), cx))
            .children(Self::detail_section("Dates", dates, cx))
            .children(Self::detail_section("Location", location, cx))
            .children(Self::detail_section("Permissions", permissions, cx))
            .into_any_element()
    }

    /// A named glyph filling the Preview tab. Used for every selection that has
    /// no decodable content — a folder, a multi-selection, nothing at all — so
    /// the tab is never empty and the shell never changes shape.
    fn render_glyph_tab(
        icon: Icon,
        title: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .size_full()
            .bg(cx.theme().muted)
            .child(empty_state(icon, title, hint, cx))
            .into_any_element()
    }

    fn render_multi(&self, entries: &[FsEntry], cx: &mut Context<Self>) -> gpui::AnyElement {
        let folders = entries.iter().filter(|e| e.is_dir()).count();
        let files = entries.len() - folders;
        let total: u64 = entries.iter().map(|e| e.size).sum();

        let contents: Vec<(SharedString, SharedString)> = vec![
            (
                "Contents".into(),
                format!("{folders} folders · {files} files").into(),
            ),
            ("Total file size".into(), format_size(total).into()),
        ];

        v_flex()
            .w_full()
            .gap_4()
            .child(Self::identity_block(
                Icon::new(PikuIcon::Copy),
                format!("{} items selected", entries.len()),
                "Multiple selection",
                cx,
            ))
            .children(Self::detail_section("General", contents, cx))
            .into_any_element()
    }

    fn render_dir_summary(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let selection = PikuState::global(cx).selection.read(cx);
        let Some(dir) = selection.dir.clone() else {
            return empty_state(
                Icon::new(PikuIcon::Info),
                "Nothing selected",
                "Select a file to see its details",
                cx,
            )
            .into_any_element();
        };
        let items = selection.dir_items;

        let general: Vec<(SharedString, SharedString)> = vec![
            ("Type".into(), "Folder".into()),
            ("Items".into(), format!("{items} visible").into()),
        ];
        let mut location: Vec<(SharedString, SharedString)> =
            vec![("Path".into(), sanitize_path(&dir).into())];
        if let Some(parent) = dir.parent() {
            location.push(("Parent folder".into(), sanitize_path(parent).into()));
        }

        let name = dir
            .file_name()
            .map(|n| crate::security::text::sanitize_label(&n.to_string_lossy()))
            .unwrap_or_else(|| sanitize_path(&dir));

        v_flex()
            .w_full()
            .gap_4()
            .child(Self::identity_block(
                Icon::new(PikuIcon::FolderOpen),
                name,
                "Current folder",
                cx,
            ))
            .children(Self::detail_section("General", general, cx))
            .children(Self::detail_section("Location", location, cx))
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
        for (index, tab) in order.iter().enumerate() {
            // The icon goes in `prefix`, not `icon`: `Tab` renders icon *or*
            // label, never both, so `.icon()` would silently drop the name and
            // leave three unlabelled glyphs. `prefix` renders beside the label.
            //
            // The active tab's glyph brightens with its label. Pinning every
            // icon to `muted_foreground` left the selected tab half-lit, which
            // read as the selection not having taken.
            let tint = match index == selected {
                true => cx.theme().foreground,
                false => cx.theme().muted_foreground,
            };
            bar = bar.child(
                Tab::new()
                    .prefix(tab.icon().size(px(14.)).text_color(tint))
                    .label(tab.label()),
            );
        }

        // `pt_2` because the header that used to supply this panel's top
        // padding is gone: the tab bar is the first thing in the dock now.
        div()
            .flex_none()
            .px_3()
            .pt_2()
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

// -- The preview's control surface ------------------------------------------
//
// Every method here has two callers: a toolbar button in `preview_view.rs` and a
// key binding routed through `ui/shell.rs`. Going through one method is what
// stops a shortcut and a click doing subtly different things — and the zoom
// methods have to live here regardless, because the geometry they need (the
// measured `preview_viewport` box, the drawn frame's size) is in this panel and
// nowhere else.
impl InspectorPanel {
    pub fn preview_zoom_by(&mut self, factor: f32, cx: &mut Context<Self>) {
        let metrics = self.drawn_metrics();
        self.view.viewport.zoom_by(factor, metrics);
        cx.notify();
    }

    /// Fitted inside the box, centred.
    pub fn preview_fit(&mut self, cx: &mut Context<Self>) {
        self.preview_set_zoom(ImageZoom::Fit, cx);
    }

    /// As wide as the box, so a page or a panorama is read by scrolling down.
    pub fn preview_fit_width(&mut self, cx: &mut Context<Self>) {
        self.preview_set_zoom(ImageZoom::FitWidth, cx);
    }

    /// One source pixel per layout pixel.
    pub fn preview_actual_size(&mut self, cx: &mut Context<Self>) {
        self.preview_set_zoom(ImageZoom::Actual, cx);
    }

    /// The three named levels go through here, and so do the toolbar's buttons.
    ///
    /// `pub(super)` while the three above are `pub`: [`ImageZoom`] belongs to the
    /// inspector's viewport engine and the shell has no business naming it.
    pub(super) fn preview_set_zoom(&mut self, zoom: ImageZoom, cx: &mut Context<Self>) {
        let metrics = self.drawn_metrics();
        self.view.viewport.set_zoom(zoom, metrics);
        cx.notify();
    }

    /// Fitted, centred and the right way up.
    ///
    /// The rotation is what makes this distinct from Fit. `set_zoom(Fit, …)`
    /// already discards the pan, so without the turn this would be the same
    /// button twice — which is what the old Reset button's comment claimed it
    /// was not.
    pub fn preview_reset_view(&mut self, cx: &mut Context<Self>) {
        self.view.viewport.reset();
        self.view.rotation = 0;
        self.sync_derived(cx);
        cx.notify();
    }

    /// Turn the frame a further 90° clockwise.
    ///
    /// Resets the viewport too: an odd turn swaps width and height, so a pan
    /// computed against the old aspect ratio points at nothing and a fit-width
    /// scale is wrong by the ratio between the two.
    pub fn preview_rotate(&mut self, cx: &mut Context<Self>) {
        if !self.has_own_pixels() {
            return;
        }
        self.view.rotation = (self.view.rotation + 1) % 4;
        self.view.viewport.reset();
        self.sync_derived(cx);
        cx.notify();
    }

    /// Step `delta` pages, clamped to the document.
    pub fn preview_step_page(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(total) = self.total_pages() else {
            return;
        };
        let last = total.saturating_sub(1);
        let next = self.view.page.saturating_add_signed(delta).min(last);
        if next == self.view.page {
            return;
        }
        self.view.page = next;
        self.sync_derived(cx);
        cx.notify();
    }

    /// Show the Preview tab, for `RevealPreview`.
    ///
    /// Revealing the dock is only half of "reveal the preview": the panel keeps
    /// whichever tab the user last chose, deliberately, so without this the
    /// quick-look gesture could open on metadata.
    pub fn show_preview_tab(&mut self, cx: &mut Context<Self>) {
        self.tab = InspectorTab::Preview;
        cx.notify();
    }

    /// Whether the loaded preview has pixels of its own — a decoded buffer with
    /// a source size.
    ///
    /// False for exactly one previewable thing: an SVG. The renderer rasterizes
    /// it from a path at whatever size it lands, so there is no source size to
    /// be 1:1 with and no buffer here for a worker to turn. Both the zoom
    /// controls that need a source size and the rotate button are disabled on
    /// it, and [`Self::preview_rotate`] refuses.
    pub(super) fn has_own_pixels(&self) -> bool {
        match self.loaded.as_ref().map(|l| &*l.content) {
            Some(PreviewContent::Image { dimensions, .. }) => dimensions.is_some(),
            Some(PreviewContent::Pdf { pages, .. }) => !pages.is_empty(),
            _ => false,
        }
    }

    /// The loaded document's page count, for a paginated preview.
    ///
    /// The *document's* count, not the loader's: the provider rasterizes one
    /// page and every other one is fetched on demand, so all of them are
    /// reachable. `None` when there is nothing to navigate — including a PDF
    /// with no decoded page, which is a message box (pdfium missing, or past the
    /// page cap) rather than a document.
    pub(super) fn total_pages(&self) -> Option<usize> {
        match self.loaded.as_ref().map(|l| &*l.content) {
            Some(PreviewContent::Pdf {
                pages, total_pages, ..
            }) if !pages.is_empty() => Some(*total_pages),
            _ => None,
        }
    }

    /// The derived frame held for the file that is open, if any.
    ///
    /// Identity is the **path** only, deliberately: while a newer frame renders,
    /// this keeps returning the older one so an image stays visible at its
    /// previous orientation instead of flashing back to the unrotated payload.
    /// A caller that cannot tolerate a stale frame checks the spec itself —
    /// `pdf_block` does, because the toolbar's counter names the page and
    /// drawing a different one under that label would be a lie.
    pub(super) fn drawn_frame(&self) -> Option<&DerivedFrame> {
        let loaded = self.loaded.as_ref()?;
        let frame = self.derived.as_ref()?;
        (frame.path == loaded.path).then_some(frame)
    }

    /// The metrics for whatever the Preview tab is drawing.
    ///
    /// One function for the body, the toolbar readout and the keyboard, so the
    /// three cannot disagree about what "100 %" means. It replaced a free
    /// function each caller passed its own idea of the dimensions to.
    pub(super) fn drawn_metrics(&self) -> Option<Metrics> {
        Metrics::new(self.preview_viewport.get(), self.drawn_dimensions()?)
    }

    /// The size of the frame on screen, in the units the zoom readout reports.
    ///
    /// Read directly by that readout as well as through [`Self::drawn_metrics`],
    /// because the two need it at different moments: the readout can name a size
    /// on the very first frame, before the renderer has measured the box that
    /// `Metrics` needs.
    pub(super) fn drawn_dimensions(&self) -> Option<(u32, u32)> {
        let loaded = self.loaded.as_ref()?;
        match &*loaded.content {
            // *Source* pixels, with the axes swapped for an odd turn — so
            // "100 %" keeps meaning one source pixel per layout pixel however
            // the frame is turned. Reporting the rotated buffer's own size
            // instead would make one zoom level mean two different things,
            // because that buffer is re-capped at `PREVIEW_MAX_EDGE`.
            PreviewContent::Image { dimensions, .. } => {
                let turns = self
                    .drawn_frame()
                    .map_or(0, |frame| frame.spec.quarter_turns());
                Some(oriented((*dimensions)?, turns))
            }
            // A page's raster *is* its source: pdfium applied both the turn and
            // the width constraint, so there is nothing left to swap.
            PreviewContent::Pdf { pages, .. } => match self.drawn_frame() {
                Some(frame) => Some(frame.dimensions),
                None => pages.first().map(|page| {
                    let size = page.size(0);
                    (u32::from(size.width), u32::from(size.height))
                }),
            },
            _ => None,
        }
    }

    /// The frame the view state is asking for, or `None` when that is the cached
    /// payload — page 0, unrotated.
    fn wanted_frame(&self) -> Option<(PathBuf, FrameSpec)> {
        let loaded = self.loaded.as_ref()?;
        let quarter_turns = self.view.rotation % 4;
        let spec = match &*loaded.content {
            PreviewContent::Pdf {
                pages, total_pages, ..
            } if !pages.is_empty() => {
                let index = self.view.page.min(total_pages.saturating_sub(1));
                if index == 0 && quarter_turns == 0 {
                    return None;
                }
                FrameSpec::PdfPage {
                    index,
                    quarter_turns,
                }
            }
            // An SVG has no buffer to turn, which `dimensions: None` is exactly
            // the marker for.
            PreviewContent::Image {
                dimensions: Some(_),
                ..
            } if quarter_turns != 0 => FrameSpec::RotatedImage { quarter_turns },
            _ => return None,
        };
        Some((loaded.path.clone(), spec))
    }

    /// Bring the derived slot in line with the view state, fetching a frame if
    /// the one wanted is not already held or already on its way.
    ///
    /// Called from the handlers that change page or rotation and never from
    /// `render`: spawning work during a paint would turn every re-render into a
    /// request.
    fn sync_derived(&mut self, cx: &mut Context<Self>) {
        match self.wanted_frame() {
            Some((path, spec)) => self.request_frame(path, spec, cx),
            // Back to the cached payload, so the frame has no remaining reader.
            None => self.release_derived(cx),
        }
    }

    fn request_frame(&mut self, path: PathBuf, spec: FrameSpec, cx: &mut Context<Self>) {
        let held = |frame: &DerivedFrame| frame.path == path && frame.spec == spec;
        if self.derived.as_ref().is_some_and(held)
            || self.derived_pending.as_ref() == Some(&(path.clone(), spec))
        {
            return;
        }

        self.derived_pending = Some((path.clone(), spec));
        let for_frame = path.clone();
        // Assigning drops the previous Inflight, which stops the worker
        // rendering the frame this one supersedes.
        self.derived_req = Some(cx.backend_task_cancellable(
            move |backend| backend.preview().derive_frame(path, spec),
            move |this: &mut Self, result, cx| {
                this.derived_req = None;
                this.derived_pending = None;
                let raw = match result {
                    Ok(Ok(raw)) => raw,
                    // Superseded or shutting down: a newer request owns the slot.
                    Ok(Err(error)) if error.is_cancelled() => return,
                    Err(error) if error.is_cancelled() => return,
                    // A page or a turn that will not render. Put the view state
                    // back to what is still on screen rather than leaving a
                    // counter claiming a page nobody can see; no error box,
                    // because the preview itself is intact.
                    Ok(Err(_)) | Err(_) => {
                        this.snap_view_to_drawn_frame();
                        cx.notify();
                        return;
                    }
                };

                // The selection can move on between the worker finishing and
                // this landing, in which case the frame arrives for a file that
                // is no longer open: nothing would ever draw it and nothing
                // would ever free it.
                if this.loaded.as_ref().map(|l| l.path.as_path()) != Some(for_frame.as_path()) {
                    let image = crate::preview::image_util::render_image_from_bgra(raw);
                    cx.drop_image(image, None);
                    return;
                }

                let dimensions = (raw.width, raw.height);
                let image = crate::preview::image_util::render_image_from_bgra(raw);
                if let Some(stale) = this.derived.replace(DerivedFrame {
                    path: for_frame,
                    spec,
                    dimensions,
                    image,
                }) {
                    // Every replacement releases. Nothing else frees the tile.
                    cx.drop_image(stale.image, None);
                }
                cx.notify();
            },
        ));
    }

    /// Drop the held frame and stop any request for one.
    ///
    /// The image is handed to `drop_image` rather than merely dropped: an
    /// `Arc<RenderImage>` owns a sprite-atlas tile and nothing else frees it.
    /// `None` for the window because this runs from the foreground executor as
    /// well as from window updates, and every window that could have painted the
    /// frame is reachable from `App` — the same reasoning as `cache_preview` in
    /// `services/preview_cache.rs`.
    fn release_derived(&mut self, cx: &mut App) {
        self.derived_req = None;
        self.derived_pending = None;
        if let Some(frame) = self.derived.take() {
            cx.drop_image(frame.image, None);
        }
    }

    /// Put `page` and `rotation` back to whatever the frame on screen actually
    /// is, after a request for a different one failed.
    fn snap_view_to_drawn_frame(&mut self) {
        let (page, rotation) = match self.drawn_frame().map(|frame| frame.spec) {
            Some(FrameSpec::PdfPage {
                index,
                quarter_turns,
            }) => (index, quarter_turns),
            Some(FrameSpec::RotatedImage { quarter_turns }) => (self.view.page, quarter_turns),
            // Nothing derived is showing, so what is showing is the cached
            // payload: page 0, unrotated.
            None => (0, 0),
        };
        self.view.page = page;
        self.view.rotation = rotation;
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
                    0 => match PikuState::global(cx).selection.read(cx).dir.clone() {
                        Some(dir) => Self::render_glyph_tab(
                            Icon::new(PikuIcon::FolderOpen),
                            dir.file_name()
                                .map(|n| {
                                    crate::security::text::sanitize_label(&n.to_string_lossy())
                                })
                                .unwrap_or_else(|| sanitize_path(&dir)),
                            "Select a file to preview it",
                            cx,
                        ),
                        None => empty_state(
                            Icon::new(PikuIcon::Info),
                            "Nothing selected",
                            "Select a file to preview it",
                            cx,
                        )
                        .into_any_element(),
                    },
                    1 => self.render_preview_tab(&entries[0], window, cx),
                    count => Self::render_glyph_tab(
                        Icon::new(PikuIcon::Copy),
                        format!("{count} items selected"),
                        "Select one file to preview it",
                        cx,
                    ),
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
            // Declares the panel's own binding scope and puts its focus handle
            // on the focus path. The preview *shortcuts* are bound globally and
            // handled on the workspace root, because while browsing it is the
            // file list that holds focus and gpui resolves actions along the
            // focus path only — so a handler here could never fire for them.
            // What this context is for is the bindings that should apply when
            // the panel itself has focus: `left`/`right` page a document, the
            // way they do in every viewer.
            //
            // Tracking focus does not change who has it on a click. The dock's
            // `TabPanel` already tracks *this* panel's handle on its own root
            // and focuses it when the tab activates, so clicking into the right
            // dock has always moved focus here.
            .key_context(crate::app::actions::INSPECTOR_CONTEXT)
            .track_focus(&self.focus_handle)
            .bg(cx.theme().sidebar)
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

    /// "Inspector", not "Details": Details is one of three tabs inside it, and
    /// naming the whole panel after one tab made the dock header disagree with
    /// the tab bar under it.
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Inspector"
    }

    /// Register with [`PikuState`] so the shell can route the preview shortcuts
    /// to this panel.
    ///
    /// `on_added_to` rather than the shell keeping the entity it built: on a
    /// persisted-layout startup this panel is constructed by the `register_panel`
    /// deserializer in `ui/mod.rs`, not by `Workspace::reset_default_layout`, so
    /// the shell has nothing to keep. The `TabPanel` calls this either way. Same
    /// arrangement `ExplorerPanel` and `NavPanel` already use.
    fn on_added_to(&mut self, _: WeakEntity<TabPanel>, _: &mut Window, cx: &mut Context<Self>) {
        PikuState::global(cx).set_inspector(cx.entity().downgrade());
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> Option<PanelControl> {
        None
    }
}
