//! One explorer pane: a self-contained workspace session with its own
//! directory, history, listing, selection, filter, sort, and watcher.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use gpui::ScrollStrategy;
use gpui::{
    App, AppContext as _, ClickEvent, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, VirtualListScrollHandle, WindowExt as _,
    dock::{Panel, PanelEvent, PanelState, TabPanel},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt as _, PopupMenu},
    v_flex,
};

use crate::services::search::{self, SearchUpdate};

use crate::storage::provider::StorageProvider as _;

use crate::app::actions::{self as actions};
use crate::app::assets::PikuIcon;
use crate::backend::services::transfer::conflict::ConflictPolicy;
use crate::core::entry::FsEntry;
use crate::security::file_name::validate_name;
use crate::security::text::{sanitize_label, sanitize_path};
use crate::services::watcher::DirWatcher;
use crate::state::PikuState;
use crate::state::pane_state::{
    MAX_HISTORY, PaneSession, SortBy, ViewMode, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP,
};
use crate::ui::components::empty_state;

/// What an open inline editor is doing. Rename anchors to a row; create is a
/// pinned editor row above the listing (no modal dialogs — VS Code style).
pub(super) enum InlineEditKind {
    Rename { ix: usize, path: PathBuf },
    Create { directory: bool },
}

pub(super) struct InlineEdit {
    pub(super) kind: InlineEditKind,
    pub(super) input: gpui::Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// The payload carried while dragging files. A plain `'static` value (gpui's
/// drag/drop is type-keyed on it); the preview view is built separately.
///
/// There is deliberately no source-tab id: `drop_into` recognizes a no-op by
/// comparing each dragged path's parent against the destination, which covers
/// same-tab drops and same-directory drops in a split alike.
#[derive(Clone)]
pub(super) struct DraggedPaths {
    pub(super) paths: Vec<PathBuf>,
}

/// The little chip shown under the cursor while dragging files.
pub(super) struct DragPreview {
    pub(super) label: SharedString,
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_2()
            .py_1()
            .gap_1()
            .items_center()
            .rounded(cx.theme().radius)
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_color(cx.theme().popover_foreground)
            .text_xs()
            .child(
                Icon::new(IconName::File)
                    .size(gpui::px(12.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(self.label.clone())
    }
}

pub struct ExplorerPanel {
    pub(super) focus_handle: FocusHandle,
    pub(super) session: PaneSession,
    pub(super) all_entries: Vec<FsEntry>,
    pub(super) entries: Vec<FsEntry>,
    pub(super) selected: BTreeSet<usize>,
    pub(super) anchor: Option<usize>,
    pub(super) back_stack: Vec<PathBuf>,
    pub(super) fwd_stack: Vec<PathBuf>,
    pub(super) filter_input: gpui::Entity<InputState>,
    pub(super) inline_edit: Option<InlineEdit>,
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    generation: u64,
    watcher: Option<DirWatcher>,
    /// The directory `watcher` is registered on. A reload triggered by a
    /// filesystem event must not tear the watcher down and build a new one —
    /// registration is a blocking syscall, and in a busy directory that ran
    /// once per event.
    watched: Option<PathBuf>,
    /// Bumped only when the watcher is actually re-registered, so the drain
    /// task's staleness check is independent of the listing generation.
    watch_gen: u64,
    /// Directory whose watch registration failed, so it is not retried on
    /// every reload. Cleared whenever the watched directory changes.
    watch_failed_for: Option<PathBuf>,
    pub(super) scroll: VirtualListScrollHandle,
    pub(super) tab_panel: Option<WeakEntity<TabPanel>>,
    /// True while showing recursive-search results instead of the folder
    /// listing. In this mode `entries` holds the accumulated matches.
    pub(super) searching: bool,
    /// Whether the current search finished (vs. still streaming).
    pub(super) search_complete: bool,
    /// Cancels the in-flight search worker when a new one starts.
    search_cancel: Arc<AtomicBool>,
    /// Bumped per search so late streamed results from a superseded search are
    /// ignored.
    search_gen: u64,
    /// Selection paths to restore after the first listing loads (session restore).
    pending_selection: Vec<PathBuf>,
    /// Row to scroll into view after restore (best-effort).
    restore_scroll: usize,
    _subscriptions: Vec<Subscription>,
}

impl ExplorerPanel {
    pub const PANEL_NAME: &'static str = "PikuExplorer";

    /// A fresh pane at the user's home directory.
    #[allow(dead_code)]
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::from_session(
            PaneSession::at(crate::services::fs_service::home_dir()),
            window,
            cx,
        )
    }

    pub fn from_session(
        mut session: PaneSession,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        session.zoom = session.zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        if session.id.is_empty() {
            session.id = crate::state::pane_state::new_tab_id();
        }

        // Restored paths are re-authorized and must still exist; anything
        // that fails is silently dropped. The stacks live on the panel — the
        // session copies are only a serialization vehicle (see `dump`).
        let guard = crate::storage::local().guard().clone();
        let sane = |p: &PathBuf| guard.sanitize(p).ok().filter(|p| p.is_dir());
        let back_stack: Vec<PathBuf> = session.back_stack.iter().filter_map(&sane).collect();
        let fwd_stack: Vec<PathBuf> = session.fwd_stack.iter().filter_map(&sane).collect();
        session.back_stack = Vec::new();
        session.fwd_stack = Vec::new();
        if sane(&session.cwd).is_none() {
            session.cwd = crate::services::fs_service::home_dir();
        }

        // Selected paths to restore once the first listing lands. Re-authorize
        // each; anything that no longer resolves is dropped.
        let pending_selection: Vec<PathBuf> = std::mem::take(&mut session.selection)
            .iter()
            .filter_map(|p| guard.sanitize(p).ok())
            .filter(|p| p.exists())
            .collect();
        let restore_scroll = session.scroll_first;

        // Seed the text box from whichever query the session persisted for its
        // current mode.
        let (initial_text, placeholder) = if session.search_deep {
            (session.search_query.clone(), "Search subfolders…")
        } else {
            (session.filter.clone(), "Filter…")
        };
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(initial_text)
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.on_query_changed(cx);
                }
            },
        ));

        let settings = PikuState::global(cx).settings.clone();
        subscriptions.push(cx.observe_in(&settings, window, |this, _, window, cx| {
            this.reload(window, cx);
        }));

        // Re-render when a thumbnail finishes decoding so the freshly cached
        // image replaces its placeholder glyph.
        let thumbnails = PikuState::global(cx).thumbnails.clone();
        subscriptions.push(cx.observe(&thumbnails, |_, _, cx| cx.notify()));

        // Re-render when git status lands so file rows pick up their badges.
        let git = PikuState::global(cx).git.clone();
        subscriptions.push(cx.observe(&git, |_, _, cx| cx.notify()));

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            session,
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            back_stack,
            fwd_stack,
            filter_input,
            inline_edit: None,
            loading: true,
            error: None,
            generation: 0,
            watcher: None,
            watched: None,
            watch_gen: 0,
            watch_failed_for: None,
            scroll: VirtualListScrollHandle::new(),
            tab_panel: None,
            searching: false,
            search_complete: false,
            search_cancel: Arc::new(AtomicBool::new(false)),
            search_gen: 0,
            pending_selection,
            restore_scroll,
            _subscriptions: subscriptions,
        };
        this.load(true, window, cx);
        // A restored deep-search tab re-runs its query (results are never
        // serialized). The folder load above still populates `all_entries` so
        // leaving search mode shows the folder instantly.
        if this.session.search_deep && !this.session.search_query.trim().is_empty() {
            this.run_search(cx);
        }
        this
    }

    pub fn tab_panel(&self) -> Option<WeakEntity<TabPanel>> {
        self.tab_panel.clone()
    }

    /// The tab title. Sanitized: a directory name is filesystem-supplied, and
    /// the tab strip is one of the few places a name is drawn without any
    /// surrounding path to give it context.
    pub fn folder_name(&self) -> SharedString {
        self.session
            .cwd
            .file_name()
            .map(|n| sanitize_label(&n.to_string_lossy()))
            .unwrap_or_else(|| {
                sanitize_path(&self.session.cwd)
                    .trim_end_matches('\\')
                    .to_string()
            })
            .into()
    }

    // -- Navigation --------------------------------------------------------

    pub fn navigate_to(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = match crate::storage::local().guard().sanitize(&path) {
            Ok(path) => path,
            Err(error) => {
                window.push_notification(crate::ui::toast::error(error.to_string()), cx);
                return;
            }
        };
        if path == self.session.cwd {
            return;
        }
        // Opening a folder (including a search result) drops search mode so the
        // destination folder is shown normally.
        if self.searching {
            self.leave_search_mode(window, cx);
        }
        self.back_stack.push(self.session.cwd.clone());
        self.fwd_stack.clear();
        self.session.cwd = path;
        self.record_recent(cx);
        self.load(true, window, cx);
    }

    pub fn go_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(previous) = self.back_stack.pop() {
            self.fwd_stack.push(self.session.cwd.clone());
            self.session.cwd = previous;
            self.load(true, window, cx);
        }
    }

    pub fn go_forward(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(next) = self.fwd_stack.pop() {
            self.back_stack.push(self.session.cwd.clone());
            self.session.cwd = next;
            self.load(true, window, cx);
        }
    }

    pub fn go_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.session.cwd.parent().map(|p| p.to_path_buf()) {
            self.navigate_to(parent, window, cx);
        }
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load(false, window, cx);
    }

    fn record_recent(&self, cx: &mut Context<Self>) {
        let cwd = self.session.cwd.clone();
        PikuState::global(cx).nav.clone().update(cx, |nav, cx| {
            nav.push_recent(cwd, cx);
        });
    }

    // -- Loading -----------------------------------------------------------

    fn load(&mut self, reset_selection: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.error = None;
        cx.notify();

        // Remember what was selected so silent refreshes keep the selection.
        let keep: Vec<PathBuf> = if reset_selection {
            Vec::new()
        } else {
            self.selected_entries()
                .iter()
                .map(|e| e.path.clone())
                .collect()
        };

        let path = self.session.cwd.clone();
        let task = cx
            .background_executor()
            .spawn(async move { crate::storage::local().list(&path) });

        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                match result {
                    Ok(entries) => {
                        this.all_entries = entries;
                        this.error = None;
                        // In search mode the displayed `entries` are match
                        // results — don't overwrite them with the folder view.
                        if !this.searching {
                            this.apply_view(cx);
                            if !keep.is_empty() {
                                this.restore_selection(&keep, cx);
                            } else if !this.pending_selection.is_empty() {
                                let restore = std::mem::take(&mut this.pending_selection);
                                this.restore_selection(&restore, cx);
                                this.scroll.scroll_to_item(
                                    this.restore_scroll
                                        .min(this.entries.len().saturating_sub(1)),
                                    ScrollStrategy::Top,
                                );
                                this.restore_scroll = 0;
                            }
                        }
                        this.start_watch(window, cx);
                        // Passive git discovery: the store caches negative
                        // results, so this is a no-op for non-repo folders.
                        let cwd = this.session.cwd.clone();
                        PikuState::global(cx)
                            .git
                            .clone()
                            .update(cx, |git, cx| git.note_dir(cwd, cx));
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        this.all_entries.clear();
                        this.entries.clear();
                        this.selected.clear();
                        this.push_selection_ctx(cx);
                        // Drop the watcher too. Leaving it bound to the
                        // previous directory means every unrelated event
                        // there re-lists the directory we failed to open —
                        // forever, since only a *successful* load re-arms.
                        this.stop_watch();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Recompute the visible listing from the raw one: hidden-file filter,
    /// text filter, then sort (directories always group first).
    pub(super) fn apply_view(&mut self, cx: &mut Context<Self>) {
        let show_hidden = PikuState::global(cx).settings.read(cx).show_hidden;
        let filter = self.filter_input.read(cx).value().to_lowercase();

        let mut entries: Vec<FsEntry> = self
            .all_entries
            .iter()
            .filter(|entry| show_hidden || !entry.hidden)
            .filter(|entry| filter.is_empty() || entry.name.to_lowercase().contains(&filter))
            .cloned()
            .collect();

        let sort = self.session.sort;
        let ascending = self.session.ascending;
        entries.sort_by(|a, b| {
            b.is_dir().cmp(&a.is_dir()).then_with(|| {
                let ordering = match sort {
                    SortBy::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                    SortBy::Size => a.size.cmp(&b.size),
                    SortBy::Modified => a.modified.cmp(&b.modified),
                    SortBy::Type => a
                        .ext
                        .cmp(&b.ext)
                        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
                };
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            })
        });

        // Selection indices are no longer meaningful after refiltering, and
        // neither is an in-place rename editor anchored to a row index.
        if matches!(
            self.inline_edit,
            Some(InlineEdit {
                kind: InlineEditKind::Rename { .. },
                ..
            })
        ) {
            self.inline_edit = None;
        }
        self.selected.clear();
        self.anchor = None;
        self.entries = entries;
        self.push_selection_ctx(cx);
        cx.notify();
    }

    fn restore_selection(&mut self, keep: &[PathBuf], cx: &mut Context<Self>) {
        if keep.is_empty() {
            return;
        }
        self.selected = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| keep.contains(&entry.path))
            .map(|(ix, _)| ix)
            .collect();
        self.push_selection_ctx(cx);
    }

    // -- Watching ----------------------------------------------------------

    /// Tear down the current watcher and retire its drain task.
    ///
    /// Bumping `watch_gen` is what stops the old task: it compares the
    /// generation it captured and exits when it no longer matches.
    fn stop_watch(&mut self) {
        self.watcher = None;
        self.watched = None;
        self.watch_failed_for = None;
        self.watch_gen = self.watch_gen.wrapping_add(1);
    }

    /// Ensure a watcher is registered on the current directory.
    ///
    /// Called after every load, including the reloads that watcher events
    /// themselves trigger — so it must be idempotent. Re-registering on each
    /// event meant a blocking `inotify_add_watch` (and a fresh drain task) for
    /// every file touched in the folder; in a home directory that fired about
    /// once a second at rest.
    fn start_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cwd = self.session.cwd.as_path();
        // Already watching this directory: nothing to do.
        if self.watcher.is_some() && self.watched.as_deref() == Some(cwd) {
            return;
        }
        // Registration already failed for this directory. Without this the
        // guard above can never short-circuit (it requires `watcher.is_some()`)
        // and every reload retries a blocking `inotify_add_watch` — permanent
        // on a machine at its watch limit or on a mount `notify` cannot watch.
        if self.watch_failed_for.as_deref() == Some(cwd) {
            return;
        }

        self.stop_watch();
        let watch_gen = self.watch_gen;

        let Ok((watcher, mut rx)) = DirWatcher::watch(&self.session.cwd) else {
            self.watch_failed_for = Some(self.session.cwd.clone());
            return;
        };
        self.watcher = Some(watcher);
        self.watched = Some(self.session.cwd.clone());

        cx.spawn_in(window, async move |this, cx| {
            while rx.next().await.is_some() {
                // Debounce bursts of filesystem events.
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                while rx.try_recv().is_ok() {}

                // Stale only once a *different* directory has been watched —
                // not merely because the listing reloaded.
                let stale = this
                    .update_in(cx, |this, window, cx| {
                        if this.watch_gen != watch_gen {
                            true
                        } else {
                            this.reload(window, cx);
                            false
                        }
                    })
                    .unwrap_or(true);
                if stale {
                    break;
                }
            }
        })
        .detach();
    }

    // -- Search ------------------------------------------------------------

    /// The text box changed. Interpret it as a recursive query (deep mode) or
    /// an in-view filter, and persist it into the session.
    fn on_query_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.filter_input.read(cx).value().to_string();
        if self.session.search_deep {
            self.session.search_query = text;
            self.run_search(cx);
        } else {
            self.session.filter = text;
            self.apply_view(cx);
        }
    }

    /// Toggle recursive search on/off (the toolbar magnifier). Carries the
    /// current text across as the query / filter and persists the mode.
    pub(super) fn toggle_deep_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.session.search_deep = !self.session.search_deep;
        let text = self.filter_input.read(cx).value().to_string();
        let placeholder = if self.session.search_deep {
            "Search subfolders…"
        } else {
            "Filter…"
        };
        self.filter_input.update(cx, |input, cx| {
            input.set_placeholder(placeholder, window, cx)
        });
        if self.session.search_deep {
            self.session.search_query = text;
            self.run_search(cx);
        } else {
            self.session.filter = text;
            self.leave_search_mode(window, cx);
        }
        // Persist the mode change with the next layout save.
        cx.emit(PanelEvent::LayoutChanged);
    }

    /// Whether recursive search is the active display mode.
    pub(super) fn is_deep_search(&self) -> bool {
        self.session.search_deep
    }

    /// Start (or restart) the background recursive search from the current
    /// query. Supersedes any in-flight search.
    fn run_search(&mut self, cx: &mut Context<Self>) {
        // Invalidate any prior search and its streamed results.
        self.search_gen = self.search_gen.wrapping_add(1);
        let generation = self.search_gen;
        self.search_cancel.store(true, Ordering::Relaxed);
        let cancel = Arc::new(AtomicBool::new(false));
        self.search_cancel = cancel.clone();

        self.searching = true;
        self.search_complete = false;
        self.entries.clear();
        self.selected.clear();
        self.anchor = None;
        self.push_selection_ctx(cx);
        cx.notify();

        let query = self.session.search_query.clone();
        if query.trim().is_empty() {
            self.search_complete = true;
            return;
        }
        let root = self.session.cwd.clone();
        let mut rx = search::start(root, query, cancel, cx.background_executor().clone());
        cx.spawn(async move |this, cx| {
            while let Some(update) = rx.next().await {
                let stop = this
                    .update(cx, |this, cx| {
                        // A newer search (or a mode exit) superseded this one.
                        if this.search_gen != generation || !this.searching {
                            return true;
                        }
                        match update {
                            SearchUpdate::Batch(mut batch) => {
                                this.entries.append(&mut batch);
                                cx.notify();
                                false
                            }
                            // `complete` is false when the walk hit one of its
                            // caps (depth, visited dirs, hits) and stopped
                            // early — the results are a prefix, not the answer.
                            SearchUpdate::Done { complete } => {
                                this.search_complete = complete;
                                cx.notify();
                                true
                            }
                        }
                    })
                    .unwrap_or(true);
                if stop {
                    break;
                }
            }
        })
        .detach();
    }

    /// Leave search mode and restore the folder listing. Clears the query text
    /// so the folder is shown unfiltered.
    fn leave_search_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_gen = self.search_gen.wrapping_add(1);
        self.search_cancel.store(true, Ordering::Relaxed);
        self.searching = false;
        self.search_complete = false;
        self.session.search_deep = false;
        self.session.search_query.clear();
        self.session.filter.clear();
        self.filter_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.set_placeholder("Filter…", window, cx);
        });
        self.apply_view(cx);
    }

    // -- Selection ---------------------------------------------------------

    pub(super) fn selected_entries(&self) -> Vec<FsEntry> {
        self.selected
            .iter()
            .filter_map(|ix| self.entries.get(*ix))
            .cloned()
            .collect()
    }

    pub(super) fn push_selection_ctx(&self, cx: &mut Context<Self>) {
        let dir = self.session.cwd.clone();
        let count = self.entries.len();
        let entries = self.selected_entries();
        PikuState::global(cx)
            .selection
            .clone()
            .update(cx, |selection, cx| {
                selection.dir = Some(dir);
                selection.dir_items = count;
                selection.entries = entries;
                cx.notify();
            });
    }

    pub(super) fn click_select(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        let modifiers = event.modifiers();
        if modifiers.control {
            if !self.selected.remove(&ix) {
                self.selected.insert(ix);
            }
            self.anchor = Some(ix);
        } else if modifiers.shift {
            let anchor = self.anchor.unwrap_or(ix);
            let (from, to) = if anchor <= ix {
                (anchor, ix)
            } else {
                (ix, anchor)
            };
            self.selected = (from..=to).collect();
        } else {
            self.selected.clear();
            self.selected.insert(ix);
            self.anchor = Some(ix);
        }
        self.push_selection_ctx(cx);
        cx.notify();
    }

    pub(super) fn select_only(&mut self, ix: usize, cx: &mut Context<Self>) {
        if !self.selected.contains(&ix) {
            self.selected.clear();
            self.selected.insert(ix);
            self.anchor = Some(ix);
            self.push_selection_ctx(cx);
            cx.notify();
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        let current = self.anchor.or_else(|| self.selected.iter().next().copied());
        let next = match current {
            Some(ix) => (ix as isize + delta).clamp(0, self.entries.len() as isize - 1) as usize,
            None => 0,
        };
        self.selected.clear();
        self.selected.insert(next);
        self.anchor = Some(next);
        self.scroll.scroll_to_item(next, ScrollStrategy::Center);
        self.push_selection_ctx(cx);
        cx.notify();
    }

    // -- Opening -----------------------------------------------------------

    pub(super) fn open_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(ix).cloned() else {
            return;
        };
        if entry.is_dir() {
            self.navigate_to(entry.path, window, cx);
        } else if crate::core::file_type::RISKY_OPEN_EXTS.contains(&entry.ext.as_str()) {
            // Executables and scripts run when shell-opened — confirm first.
            super::dialogs::confirm_open_executable(entry, window, cx);
        } else {
            shell_open(&entry.name, &entry.path, window, cx);
        }
    }

    /// Double-click behavior: folders navigate; any file reveals the inspector
    /// preview (the selection feeds it). Unlike [`open_entry`], this never
    /// launches the OS handler — external open stays on Enter / context menu,
    /// so double-clicking can never accidentally execute a program.
    pub(super) fn open_or_preview_entry(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.entries.get(ix).cloned() else {
            return;
        };
        if entry.is_dir() {
            self.navigate_to(entry.path, window, cx);
        } else {
            self.select_only(ix, cx);
            window.dispatch_action(Box::new(actions::RevealPreview), cx);
        }
    }

    // -- Clipboard / operations -------------------------------------------

    fn copy_selection(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self
            .selected_entries()
            .iter()
            .map(|e| e.path.clone())
            .collect();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        PikuState::global(cx)
            .clipboard
            .clone()
            .update(cx, |clipboard, _| {
                clipboard.paths = paths;
                clipboard.cut = cut;
            });
        window.push_notification(
            crate::ui::toast::info(format!(
                "{} {count} item{} — paste with Ctrl+V",
                if cut { "Cut" } else { "Copied" },
                if count == 1 { "" } else { "s" }
            )),
            cx,
        );
    }

    fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = PikuState::global(cx);
        let clipboard = state.clipboard.clone();
        let jobs = state.jobs.clone();
        let (paths, cut) = {
            let clip = clipboard.read(cx);
            (clip.paths.clone(), clip.cut)
        };
        if paths.is_empty() {
            return;
        }
        let dest = self.session.cwd.clone();
        jobs.update(cx, |jobs, cx| {
            // `Ask`: a paste over an existing tree stops and shows the
            // collisions once, rather than silently writing `name (2)` beside
            // every one of them. The user's answer becomes the job's policy from
            // there on.
            jobs.submit_copy(paths, dest, cut, ConflictPolicy::Ask, window, cx);
        });
        if cut {
            clipboard.update(cx, |clipboard, _| {
                clipboard.paths.clear();
                clipboard.cut = false;
            });
        }
    }

    /// `delete` — to the Recycle Bin, and skippable via `confirm_delete`.
    fn delete_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.selected_entries();
        if entries.is_empty() {
            return;
        }
        let confirm = PikuState::global(cx).settings.read(cx).confirm_delete;
        if confirm {
            super::dialogs::confirm_delete(entries, window, cx);
        } else {
            let paths = entries.into_iter().map(|e| e.path).collect();
            PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                jobs.submit_delete(paths, window, cx);
            });
        }
    }

    /// `shift-delete` — no trash, no undo, and always a dialog.
    ///
    /// `settings.confirm_delete` is not consulted: it is the user's answer about
    /// the reversible flow above, and there is no unattended path to permanent
    /// destruction. See [`super::dialogs::confirm_delete_permanent`].
    fn delete_permanent_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.selected_entries();
        if entries.is_empty() {
            return;
        }
        super::dialogs::confirm_delete_permanent(entries, window, cx);
    }

    fn toggle_view(&mut self, cx: &mut Context<Self>) {
        self.session.view = match self.session.view {
            ViewMode::List => ViewMode::Grid,
            ViewMode::Grid => ViewMode::List,
        };
        cx.emit(PanelEvent::LayoutChanged);
        cx.notify();
    }

    pub(super) fn set_sort(&mut self, sort: SortBy, cx: &mut Context<Self>) {
        if self.session.sort == sort {
            self.session.ascending = !self.session.ascending;
        } else {
            self.session.sort = sort;
            self.session.ascending = true;
        }
        self.apply_view(cx);
    }

    // -- Zoom --------------------------------------------------------------

    pub(super) fn zoom(&self) -> f32 {
        self.session.zoom
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        // Snap to the step grid so repeated ctrl+wheel ticks never drift.
        let zoom = ((zoom / ZOOM_STEP).round() * ZOOM_STEP).clamp(ZOOM_MIN, ZOOM_MAX);
        if (zoom - self.session.zoom).abs() > f32::EPSILON {
            self.session.zoom = zoom;
            // Rides the dock layout save, which persists the pane session.
            cx.emit(PanelEvent::LayoutChanged);
            cx.notify();
        }
    }

    fn zoom_by(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.set_zoom(self.session.zoom + delta, cx);
    }

    // -- Inline editing ----------------------------------------------------

    /// Index of the row currently being renamed in place, if any.
    pub(super) fn renaming_ix(&self) -> Option<usize> {
        match &self.inline_edit {
            Some(InlineEdit {
                kind: InlineEditKind::Rename { ix, .. },
                ..
            }) => Some(*ix),
            _ => None,
        }
    }

    fn start_inline_edit(
        &mut self,
        kind: InlineEditKind,
        initial: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inline_edit.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(initial.to_string())
                .placeholder(placeholder)
        });
        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // Enter is the only commit; losing focus cancels, so a stray
                // click can never mutate the filesystem.
                InputEvent::PressEnter { .. } => this.commit_inline_edit(window, cx),
                InputEvent::Blur => this.cancel_inline_edit(cx),
                _ => {}
            },
        ));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.inline_edit = Some(InlineEdit {
            kind,
            input,
            _subs: subs,
        });
        cx.notify();
    }

    pub(super) fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inline_edit.is_some() {
            return;
        }
        let selected = self.selected_entries();
        let Some(entry) = selected.first().cloned() else {
            return;
        };
        if selected.len() > 1 {
            window.push_notification(crate::ui::toast::info("Select a single item to rename"), cx);
            return;
        }
        let Some(ix) = self.entries.iter().position(|e| e.path == entry.path) else {
            return;
        };
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
        self.start_inline_edit(
            InlineEditKind::Rename {
                ix,
                path: entry.path.clone(),
            },
            &entry.name,
            "New name",
            window,
            cx,
        );
    }

    pub(super) fn start_create(
        &mut self,
        directory: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.inline_edit.is_some() {
            return;
        }
        self.start_inline_edit(
            InlineEditKind::Create { directory },
            "",
            if directory {
                "Folder name"
            } else {
                "File name"
            },
            window,
            cx,
        );
    }

    /// Close the editor without committing. Does not move focus — blur-cancel
    /// must never steal focus from whatever the user clicked into.
    pub(super) fn cancel_inline_edit(&mut self, cx: &mut Context<Self>) {
        if self.inline_edit.take().is_some() {
            cx.notify();
        }
    }

    fn commit_inline_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.inline_edit.take() else {
            return;
        };
        let name = edit.input.read(cx).value().trim().to_string();
        if name.is_empty() {
            window.focus(&self.focus_handle, cx);
            cx.notify();
            return;
        }
        if let Err(error) = validate_name(&name) {
            // Keep the editor open so the user can fix the name.
            window.push_notification(crate::ui::toast::error(error), cx);
            self.inline_edit = Some(edit);
            return;
        }
        match &edit.kind {
            InlineEditKind::Rename { path, .. } => {
                let old_path = path.clone();
                if let Some(parent) = old_path.parent() {
                    let new_path = parent.join(&name);
                    if new_path != old_path {
                        PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                            jobs.submit_rename(old_path, new_path, window, cx);
                        });
                    }
                }
            }
            InlineEditKind::Create { directory } => {
                let path = self.session.cwd.join(&name);
                let directory = *directory;
                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                    if directory {
                        jobs.submit_new_folder(path, window, cx);
                    } else {
                        jobs.submit_new_file(path, window, cx);
                    }
                });
            }
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    /// The pinned editor row shown above the listing while creating a folder
    /// or file. Same 30px metrics as a list row, same icon language.
    fn render_create_row(
        &self,
        directory: bool,
        input: gpui::Entity<InputState>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .flex_none()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .on_key_down(cx.listener(Self::on_editor_key_down))
            .child(
                Icon::new(if directory {
                    IconName::Folder
                } else {
                    IconName::File
                })
                .size(gpui::px(16.))
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .rounded(cx.theme().radius)
                    .child(Input::new(&input).small()),
            )
    }

    /// Escape closes the editor and hands focus back to the pane. Wired as an
    /// `on_key_down` on the editor's wrapper (the Input's own Escape action
    /// ends in `cx.propagate()`, so the event reaches the wrapper).
    pub(super) fn on_editor_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.cancel_inline_edit(cx);
            window.focus(&self.focus_handle, cx);
        }
    }

    // -- Action handlers ---------------------------------------------------

    fn on_toggle_hidden(
        &mut self,
        _: &actions::ToggleHidden,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let settings = PikuState::global(cx).settings.clone();
        settings.update(cx, |settings, cx| {
            settings.show_hidden = !settings.show_hidden;
            settings.save();
            cx.notify();
        });
    }

    // -- Tab identity ------------------------------------------------------

    /// The tab label: the user's title override, else the folder name.
    pub(super) fn tab_title(&self) -> SharedString {
        match &self.session.title {
            Some(title) if !title.trim().is_empty() => title.clone().into(),
            _ => self.folder_name(),
        }
    }

    /// A full, independent copy of this tab's session for Duplicate Tab. Folds
    /// the live (capped) history stacks in, exactly like `dump`, then re-ids.
    pub fn duplicate_session(&self) -> PaneSession {
        self.session_for_dump().duplicate()
    }

    /// A fresh session at this pane's folder for a NEW tab: inherits the view
    /// preferences (mode / sort / zoom) but starts with clean history,
    /// selection, and search, and a brand-new identity.
    pub fn new_tab_session(&self) -> PaneSession {
        let mut session = PaneSession::at(self.session.cwd.clone());
        session.view = self.session.view;
        session.sort = self.session.sort;
        session.ascending = self.session.ascending;
        session.zoom = self.session.zoom;
        session
    }

    /// The session with live panel state (history, selection, scroll) folded
    /// back in — the single source of truth for both `dump` and duplication.
    fn session_for_dump(&self) -> PaneSession {
        fn tail(stack: &[PathBuf], cap: usize) -> Vec<PathBuf> {
            stack[stack.len().saturating_sub(cap)..].to_vec()
        }
        let mut session = self.session.clone();
        session.back_stack = tail(&self.back_stack, MAX_HISTORY);
        session.fwd_stack = tail(&self.fwd_stack, MAX_HISTORY);
        // Selection is meaningless while showing search results.
        session.selection = if self.searching {
            Vec::new()
        } else {
            self.selected_entries()
                .iter()
                .map(|e| e.path.clone())
                .collect()
        };
        session.scroll_first = self.scroll.base_handle().top_item();
        session
    }

    /// Toggle this tab's pinned state. Pinned tabs are non-closable and keep
    /// their pin badge; the change persists with the layout.
    pub fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        self.session.pinned = !self.session.pinned;
        cx.emit(PanelEvent::LayoutChanged);
        cx.notify();
    }

    // -- Drag & drop -------------------------------------------------------

    /// The paths a drag from row `ix` should carry: the whole selection if the
    /// row is part of it, otherwise just that row.
    pub(super) fn drag_paths(&self, ix: usize) -> Vec<PathBuf> {
        if self.selected.contains(&ix) && self.selected.len() > 1 {
            self.selected_entries()
                .iter()
                .map(|e| e.path.clone())
                .collect()
        } else {
            self.entries
                .get(ix)
                .map(|e| vec![e.path.clone()])
                .into_iter()
                .flatten()
                .collect()
        }
    }

    /// Receive a set of dropped paths into `dest` (move or copy). Every path is
    /// authorized again inside the transfer engine, which also records the audit
    /// entry.
    pub(super) fn drop_into(
        &mut self,
        paths: Vec<PathBuf>,
        dest: PathBuf,
        is_move: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Skip anything already in the destination directory (a no-op move, or
        // a copy that would only clash names).
        let dest_ref = dest.as_path();
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|p| p.parent() != Some(dest_ref) && p.as_path() != dest_ref)
            .collect();
        if paths.is_empty() {
            return;
        }
        PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
            jobs.submit_copy(paths, dest, is_move, ConflictPolicy::Ask, window, cx);
        });
    }

    /// Copy files dropped from outside the app (OS drag-in) into `dest`.
    pub(super) fn drop_external(
        &mut self,
        paths: Vec<PathBuf>,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // External paths are always copied (never moved out of their origin),
        // and are sanitized by the job queue before any filesystem touch.
        self.drop_into(paths, dest, false, window, cx);
    }
}

impl Render for ExplorerPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Times the pass under `PIKU_TRACE_SPANS=1`, and in debug builds makes
        // any blocking call reached from here panic instead of stuttering.
        let _pass = crate::app::diagnostics::enter_render("ExplorerPanel");

        let content = if let Some(error) = self.error.clone() {
            empty_state(
                Icon::new(IconName::TriangleAlert),
                "Could not open this folder",
                error,
                cx,
            )
            .into_any_element()
        } else if self.loading && self.entries.is_empty() {
            div()
                .size_full()
                .p_4()
                .overflow_hidden()
                .child(crate::ui::components::skeleton_rows(
                    8,
                    super::file_list::row_height(self.session.zoom),
                    cx,
                ))
                .into_any_element()
        } else if self.entries.is_empty() {
            let filtered = !self.filter_input.read(cx).value().is_empty();
            empty_state(
                Icon::new(IconName::FolderOpen),
                if filtered {
                    "No matches"
                } else {
                    "This folder is empty"
                },
                if filtered {
                    "Nothing here matches the filter"
                } else {
                    "Drop files here or create a new folder"
                },
                cx,
            )
            .into_any_element()
        } else {
            match self.session.view {
                ViewMode::List => self.render_list(window, cx).into_any_element(),
                ViewMode::Grid => self.render_grid(window, cx).into_any_element(),
            }
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .key_context(actions::EXPLORER_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &actions::NavigateBack, window, cx| this.go_back(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::NavigateForward, window, cx| {
                    this.go_forward(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::NavigateUp, window, cx| this.go_up(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::RefreshPane, window, cx| this.reload(window, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::FocusFilter, window, cx| {
                this.filter_input
                    .clone()
                    .update(cx, |input, cx| input.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::CopySelection, window, cx| {
                this.copy_selection(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &actions::CutSelection, window, cx| {
                this.copy_selection(true, window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::PasteClipboard, window, cx| this.paste(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::DeleteSelection, window, cx| {
                    this.delete_selection(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::DeletePermanentSelection, window, cx| {
                    this.delete_permanent_selection(window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &actions::RenameSelection, window, cx| {
                    this.start_rename(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &actions::NewFolder, window, cx| {
                this.start_create(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::NewFile, window, cx| {
                this.start_create(false, window, cx);
            }))
            .on_action(cx.listener(Self::on_toggle_hidden))
            .on_action(cx.listener(|this, _: &actions::ToggleViewMode, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &actions::OpenSelection, window, cx| {
                if let Some(ix) = this.selected.iter().next().copied() {
                    this.open_entry(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &actions::SelectAll, _, cx| {
                this.selected = (0..this.entries.len()).collect();
                this.push_selection_ctx(cx);
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &actions::SelectNext, _, cx| this.move_selection(1, cx)),
            )
            .on_action(
                cx.listener(|this, _: &actions::SelectPrev, _, cx| this.move_selection(-1, cx)),
            )
            .on_action(cx.listener(|this, action: &actions::SortByName, _, cx| {
                let _ = action;
                this.set_sort(SortBy::Name, cx);
            }))
            .on_action(
                cx.listener(|this, _: &actions::SortBySize, _, cx| this.set_sort(SortBy::Size, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::SortByModified, _, cx| {
                this.set_sort(SortBy::Modified, cx)
            }))
            .on_action(
                cx.listener(|this, _: &actions::SortByType, _, cx| this.set_sort(SortBy::Type, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::FavoriteSelection, _, cx| {
                if let Some(entry) = this.selected_entries().first() {
                    let path = entry.path.clone();
                    PikuState::global(cx)
                        .nav
                        .clone()
                        .update(cx, |nav, cx| nav.toggle_favorite(path, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::PinSelection, _, cx| {
                if let Some(entry) = this.selected_entries().first() {
                    let path = entry.path.clone();
                    PikuState::global(cx)
                        .nav
                        .clone()
                        .update(cx, |nav, cx| nav.toggle_pinned(path, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::ZoomIn, _, cx| this.zoom_by(ZOOM_STEP, cx)))
            .on_action(
                cx.listener(|this, _: &actions::ZoomOut, _, cx| this.zoom_by(-ZOOM_STEP, cx)),
            )
            .on_action(cx.listener(|this, _: &actions::ResetZoom, _, cx| this.set_zoom(1.0, cx)))
            .child(self.render_toolbar(window, cx))
            .children(match &self.inline_edit {
                Some(InlineEdit {
                    kind: InlineEditKind::Create { directory },
                    input,
                    ..
                }) => {
                    let (directory, input) = (*directory, input.clone());
                    Some(self.render_create_row(directory, input, cx))
                }
                _ => None,
            })
            .child(
                div()
                    .id("explorer-content")
                    .flex_1()
                    .min_h_0()
                    // Dropping onto empty space moves/copies into this folder.
                    // Ctrl forces a copy; external OS files always copy.
                    .drag_over::<DraggedPaths>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                    .on_drop(cx.listener(|this, dragged: &DraggedPaths, window, cx| {
                        let dest = this.session.cwd.clone();
                        let is_move = !window.modifiers().control;
                        this.drop_into(dragged.paths.clone(), dest, is_move, window, cx);
                    }))
                    .on_drop(
                        cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                            let dest = this.session.cwd.clone();
                            this.drop_external(paths.paths().to_vec(), dest, window, cx);
                        }),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &gpui::ScrollWheelEvent, _, cx| {
                        if !event.modifiers.control {
                            return;
                        }
                        let dy = match event.delta {
                            gpui::ScrollDelta::Pixels(delta) => f32::from(delta.y),
                            gpui::ScrollDelta::Lines(delta) => delta.y,
                        };
                        if dy > 0.0 {
                            this.zoom_by(ZOOM_STEP, cx);
                        } else if dy < 0.0 {
                            this.zoom_by(-ZOOM_STEP, cx);
                        }
                        cx.stop_propagation();
                    }))
                    .context_menu({
                        let has_selection = !self.selected.is_empty();
                        let has_clipboard =
                            !PikuState::global(cx).clipboard.read(cx).paths.is_empty();
                        move |menu, window, cx| {
                            super::context_menu::build(
                                menu,
                                has_selection,
                                has_clipboard,
                                window,
                                cx,
                            )
                        }
                    })
                    .child(content),
            )
    }
}

impl Focusable for ExplorerPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for ExplorerPanel {}

impl Panel for ExplorerPanel {
    fn panel_name(&self) -> &'static str {
        Self::PANEL_NAME
    }

    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some(self.tab_title())
    }

    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The storage-type icon leads the label. `Local` uses the folder glyph;
        // a future cloud tab would swap in a cloud icon here.
        let icon = match self.session.storage {
            crate::state::pane_state::StorageKind::Local => IconName::Folder,
        };
        gpui_component::h_flex()
            .gap_1()
            .items_center()
            .child(
                Icon::new(icon)
                    .size(gpui::px(14.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(self.tab_title())
    }

    /// Pin/search/branch badges after the tab label. Kept cheap — this runs
    /// on every tab render (branch lookup is two HashMap reads).
    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        // Active branch of the repository this pane is inside, if any —
        // "the active branch appears inside repository tabs".
        let branch: Option<String> = {
            let git = PikuState::global(cx).git.read(cx);
            git.root_for(&self.session.cwd)
                .and_then(|root| git.snapshot(root))
                .and_then(|snap| snap.branch.clone())
        };
        if !self.session.pinned && !self.searching && branch.is_none() {
            return None;
        }
        let mut row = h_flex().gap_0p5().items_center().ml_1();
        if let Some(branch) = branch {
            row = row
                .child(
                    Icon::new(PikuIcon::GitBranch)
                        .size(gpui::px(11.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .text_xs()
                        .max_w(gpui::px(90.))
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(branch),
                );
        }
        if self.session.pinned {
            row = row.child(
                Icon::new(PikuIcon::Pin)
                    .size(gpui::px(11.))
                    .text_color(cx.theme().muted_foreground),
            );
        }
        if self.searching {
            row = row.child(
                Icon::new(IconName::Search)
                    .size(gpui::px(11.))
                    .text_color(cx.theme().muted_foreground),
            );
        }
        Some(row)
    }

    /// Pinned tabs cannot be closed by accident.
    fn closable(&self, _: &App) -> bool {
        !self.session.pinned
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> PopupMenu {
        let pin_label = if self.session.pinned {
            "Unpin tab"
        } else {
            "Pin tab"
        };
        menu.menu(pin_label, Box::new(actions::PinTab))
            .menu("Duplicate tab", Box::new(actions::DuplicateTab))
    }

    fn on_added_to(
        &mut self,
        tab_panel: WeakEntity<TabPanel>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tab_panel = Some(tab_panel);
        PikuState::global(cx).set_active_explorer(cx.entity().downgrade());
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if active {
            PikuState::global(cx).set_active_explorer(cx.entity().downgrade());
            self.push_selection_ctx(cx);
        }
    }

    fn dump(&self, _cx: &App) -> PanelState {
        // Fold live panel state (capped history, selection, scroll) back into
        // the session so it all survives a restart.
        let session = self.session_for_dump();
        let mut state = PanelState::new(self);
        if let Ok(value) = serde_json::to_value(&session) {
            state.info = gpui_component::dock::PanelInfo::panel(value);
        }
        state
    }
}

/// Hand a file to the OS default handler, hardened against link tricks:
/// sanitize the lexical path, resolve junctions/symlinks with
/// `canonicalize`, then re-authorize the *real* target — a link inside an
/// allowed root must not open something outside it.
///
/// `name` is cleaned here rather than at the call sites. It ends up inside the
/// two failure toasts below, both of which quote it, and every caller passes a
/// name that came from the filesystem. Doing it once is the difference between
/// one correct place and a rule each new caller has to remember.
pub fn shell_open(name: &str, path: &Path, window: &mut Window, cx: &mut App) {
    let name = sanitize_label(name);
    let resolved = crate::storage::local()
        .guard()
        .sanitize(path)
        .map_err(|error| error.to_string())
        .and_then(|lexical| {
            std::fs::canonicalize(&lexical)
                .map_err(|error| format!("resolving {}: {error}", sanitize_path(&lexical)))
        })
        .and_then(|real| {
            crate::storage::local()
                .guard()
                .sanitize(&real)
                .map_err(|_| format!("“{name}” points outside the allowed locations"))
        });
    match resolved {
        Ok(real) => {
            if let Err(error) = open::that_detached(&real) {
                window.push_notification(
                    crate::ui::toast::error(format!("Could not open “{name}”: {error}")),
                    cx,
                );
            }
        }
        Err(error) => {
            window.push_notification(crate::ui::toast::error(error), cx);
        }
    }
}
