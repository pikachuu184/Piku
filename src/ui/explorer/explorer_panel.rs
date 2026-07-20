//! One explorer pane: a self-contained workspace session with its own
//! directory, history, listing, selection, filter, sort, and watcher.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::{
    App, AppContext as _, ClickEvent, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, WeakEntity, Window, div,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, VirtualListScrollHandle, WindowExt as _,
    dock::{Panel, PanelEvent, PanelState, TabPanel},
    input::{InputEvent, InputState},
    menu::ContextMenuExt as _,
    notification::Notification,
    v_flex,
};
use gpui::ScrollStrategy;

use crate::storage::provider::StorageProvider as _;

use crate::app::actions::{self as actions};
use crate::core::entry::FsEntry;
use crate::services::watcher::DirWatcher;
use crate::state::PikuState;
use crate::state::pane_state::{PaneSession, SortBy, ViewMode, ZOOM_MAX, ZOOM_MIN, ZOOM_STEP};
use crate::ui::components::empty_state;

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
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    generation: u64,
    watcher: Option<DirWatcher>,
    pub(super) scroll: VirtualListScrollHandle,
    pub(super) tab_panel: Option<WeakEntity<TabPanel>>,
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
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter…"));

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    this.apply_view(cx);
                }
            },
        ));

        let settings = PikuState::global(cx).settings.clone();
        subscriptions.push(cx.observe_in(&settings, window, |this, _, window, cx| {
            this.reload(window, cx);
        }));

        let mut this = Self {
            focus_handle: cx.focus_handle(),
            session,
            all_entries: Vec::new(),
            entries: Vec::new(),
            selected: BTreeSet::new(),
            anchor: None,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
            filter_input,
            loading: true,
            error: None,
            generation: 0,
            watcher: None,
            scroll: VirtualListScrollHandle::new(),
            tab_panel: None,
            _subscriptions: subscriptions,
        };
        this.load(true, window, cx);
        this
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.session.cwd
    }

    pub fn tab_panel(&self) -> Option<WeakEntity<TabPanel>> {
        self.tab_panel.clone()
    }

    pub fn folder_name(&self) -> SharedString {
        self.session
            .cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| {
                self.session
                    .cwd
                    .to_string_lossy()
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
                window.push_notification(Notification::error(error.to_string()), cx);
                return;
            }
        };
        if path == self.session.cwd {
            return;
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
            self.selected_entries().iter().map(|e| e.path.clone()).collect()
        };

        let path = self.session.cwd.clone();
        let task = cx.background_executor().spawn(async move {
            crate::storage::local().list(&path)
        });

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
                        this.apply_view(cx);
                        this.restore_selection(&keep, cx);
                        this.start_watch(window, cx);
                    }
                    Err(error) => {
                        this.error = Some(format!("{error:#}"));
                        this.all_entries.clear();
                        this.entries.clear();
                        this.selected.clear();
                        this.push_selection_ctx(cx);
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
                    SortBy::Type => a.ext.cmp(&b.ext).then_with(|| {
                        a.name.to_lowercase().cmp(&b.name.to_lowercase())
                    }),
                };
                if ascending { ordering } else { ordering.reverse() }
            })
        });

        // Selection indices are no longer meaningful after refiltering.
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

    fn start_watch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.watcher = None;
        let Ok((watcher, mut rx)) = DirWatcher::watch(&self.session.cwd) else {
            return;
        };
        self.watcher = Some(watcher);
        let generation = self.generation;

        cx.spawn_in(window, async move |this, cx| {
            while rx.next().await.is_some() {
                // Debounce bursts of filesystem events.
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                while rx.try_recv().is_ok() {}

                let stale = this
                    .update_in(cx, |this, window, cx| {
                        if this.generation != generation {
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
        PikuState::global(cx).selection.clone().update(cx, |selection, cx| {
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
            let (from, to) = if anchor <= ix { (anchor, ix) } else { (ix, anchor) };
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
        let current = self
            .anchor
            .or_else(|| self.selected.iter().next().copied());
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
        } else {
            match crate::storage::local().guard().sanitize(&entry.path) {
                Ok(path) => {
                    if let Err(error) = open::that_detached(&path) {
                        window.push_notification(
                            Notification::error(format!("Could not open “{}”: {error}", entry.name)),
                            cx,
                        );
                    }
                }
                Err(error) => {
                    window.push_notification(Notification::error(error.to_string()), cx);
                }
            }
        }
    }

    // -- Clipboard / operations -------------------------------------------

    fn copy_selection(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths: Vec<PathBuf> = self.selected_entries().iter().map(|e| e.path.clone()).collect();
        if paths.is_empty() {
            return;
        }
        let count = paths.len();
        PikuState::global(cx).clipboard.clone().update(cx, |clipboard, _| {
            clipboard.paths = paths;
            clipboard.cut = cut;
        });
        window.push_notification(
            Notification::info(format!(
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
            jobs.submit_copy(paths, dest, cut, window, cx);
        });
        if cut {
            clipboard.update(cx, |clipboard, _| {
                clipboard.paths.clear();
                clipboard.cut = false;
            });
        }
    }

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

    // -- Action handlers ---------------------------------------------------

    fn on_toggle_hidden(&mut self, _: &actions::ToggleHidden, _: &mut Window, cx: &mut Context<Self>) {
        let settings = PikuState::global(cx).settings.clone();
        settings.update(cx, |settings, cx| {
            settings.show_hidden = !settings.show_hidden;
            settings.save();
            cx.notify();
        });
    }
}

impl Render for ExplorerPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                if filtered { "No matches" } else { "This folder is empty" },
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
            .on_action(cx.listener(|this, _: &actions::NavigateBack, window, cx| this.go_back(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NavigateForward, window, cx| this.go_forward(window, cx)))
            .on_action(cx.listener(|this, _: &actions::NavigateUp, window, cx| this.go_up(window, cx)))
            .on_action(cx.listener(|this, _: &actions::RefreshPane, window, cx| this.reload(window, cx)))
            .on_action(cx.listener(|this, _: &actions::FocusFilter, window, cx| {
                this.filter_input.clone().update(cx, |input, cx| input.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &actions::CopySelection, window, cx| this.copy_selection(false, window, cx)))
            .on_action(cx.listener(|this, _: &actions::CutSelection, window, cx| this.copy_selection(true, window, cx)))
            .on_action(cx.listener(|this, _: &actions::PasteClipboard, window, cx| this.paste(window, cx)))
            .on_action(cx.listener(|this, _: &actions::DeleteSelection, window, cx| this.delete_selection(window, cx)))
            .on_action(cx.listener(|this, _: &actions::RenameSelection, window, cx| {
                super::dialogs::rename_selected(this, window, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::NewFolder, window, cx| {
                super::dialogs::new_folder(this, window, cx);
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
            .on_action(cx.listener(|this, _: &actions::SelectNext, _, cx| this.move_selection(1, cx)))
            .on_action(cx.listener(|this, _: &actions::SelectPrev, _, cx| this.move_selection(-1, cx)))
            .on_action(cx.listener(|this, action: &actions::SortByName, _, cx| {
                let _ = action;
                this.set_sort(SortBy::Name, cx);
            }))
            .on_action(cx.listener(|this, _: &actions::SortBySize, _, cx| this.set_sort(SortBy::Size, cx)))
            .on_action(cx.listener(|this, _: &actions::SortByModified, _, cx| this.set_sort(SortBy::Modified, cx)))
            .on_action(cx.listener(|this, _: &actions::SortByType, _, cx| this.set_sort(SortBy::Type, cx)))
            .on_action(cx.listener(|this, _: &actions::FavoriteSelection, _, cx| {
                if let Some(entry) = this.selected_entries().first() {
                    let path = entry.path.clone();
                    PikuState::global(cx).nav.clone().update(cx, |nav, cx| nav.toggle_favorite(path, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::PinSelection, _, cx| {
                if let Some(entry) = this.selected_entries().first() {
                    let path = entry.path.clone();
                    PikuState::global(cx).nav.clone().update(cx, |nav, cx| nav.toggle_pinned(path, cx));
                }
            }))
            .on_action(cx.listener(|this, _: &actions::ZoomIn, _, cx| this.zoom_by(ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &actions::ZoomOut, _, cx| this.zoom_by(-ZOOM_STEP, cx)))
            .on_action(cx.listener(|this, _: &actions::ResetZoom, _, cx| this.set_zoom(1.0, cx)))
            .child(self.render_toolbar(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
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
                        let has_clipboard = !PikuState::global(cx).clipboard.read(cx).paths.is_empty();
                        move |menu, window, cx| {
                            super::context_menu::build(menu, has_selection, has_clipboard, window, cx)
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
        Some(self.folder_name())
    }

    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui_component::h_flex()
            .gap_1()
            .items_center()
            .child(
                Icon::new(IconName::Folder)
                    .size(gpui::px(14.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(self.folder_name())
    }

    fn closable(&self, _: &App) -> bool {
        true
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
        let mut state = PanelState::new(self);
        if let Ok(value) = serde_json::to_value(&self.session) {
            state.info = gpui_component::dock::PanelInfo::panel(value);
        }
        state
    }
}
