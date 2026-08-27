pub mod nav_model;
pub mod pane_state;
pub mod persistence;
pub mod settings;
pub mod workspaces;

use std::cell::RefCell;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Entity, Global, WeakEntity};

use crate::core::entry::FsEntry;
use crate::services::audio_player::AudioPlayer;
use crate::services::drive_scan::DriveStatsStore;
use crate::services::git::store::GitStore;
use crate::services::jobs::JobQueue;
use crate::services::preview_cache::PreviewCache;
use crate::services::thumbnails::ThumbnailCache;
use crate::state::nav_model::NavModel;
use crate::state::settings::Settings;
use crate::state::workspaces::WorkspaceStore;
use crate::ui::explorer::ExplorerPanel;
use crate::ui::inspector::InspectorPanel;
use crate::ui::sidebar::NavPanel;

/// What the active pane currently has selected; the inspector and status bar
/// observe this entity.
#[derive(Default)]
pub struct SelectionCtx {
    pub dir: Option<PathBuf>,
    pub dir_items: usize,
    pub entries: Vec<FsEntry>,
}

/// Internal file clipboard for copy/cut/paste between panes.
#[derive(Default)]
pub struct FileClipboard {
    pub paths: Vec<PathBuf>,
    pub cut: bool,
}

pub struct PikuState {
    /// The asynchronous filesystem backend. Every view reaches the operating
    /// system through this and nothing else.
    backend: crate::backend::Backend,
    pub settings: Entity<Settings>,
    pub workspaces: Entity<WorkspaceStore>,
    pub nav: Entity<NavModel>,
    pub jobs: Entity<JobQueue>,
    pub clipboard: Entity<FileClipboard>,
    pub selection: Entity<SelectionCtx>,
    pub drive_stats: Entity<DriveStatsStore>,
    pub thumbnails: Entity<ThumbnailCache>,
    /// Decoded-preview cache shared by the inspector and the media panel, so
    /// re-opening a file already previewed is instant instead of re-decoding.
    pub preview_cache: Entity<PreviewCache>,
    pub audio: Entity<AudioPlayer>,
    /// Passive git repository monitoring; panes report visited directories
    /// and every git-aware view observes this entity.
    pub git: Entity<GitStore>,
    active_explorer: RefCell<Option<WeakEntity<ExplorerPanel>>>,
    nav_panel: RefCell<Option<WeakEntity<NavPanel>>>,
    /// The right dock's inspector, so the preview shortcuts can reach it.
    ///
    /// They are bound globally and handled on the workspace root, because gpui
    /// resolves actions along the focus path and the inspector is a *sibling* of
    /// whatever holds focus while browsing. The panel registers itself from
    /// `Panel::on_added_to`, same as the two above.
    inspector: RefCell<Option<WeakEntity<InspectorPanel>>>,
}

impl Global for PikuState {}

impl PikuState {
    /// Build the global state. Returns `false` if the backend could not start,
    /// in which case the caller should abort startup rather than run an app
    /// that cannot reach the filesystem.
    pub fn init(cx: &mut App) -> bool {
        let Some(backend) = crate::backend::Backend::new() else {
            return false;
        };
        Self::init_with(cx, backend);
        true
    }

    fn init_with(cx: &mut App, backend: crate::backend::Backend) {
        let settings = cx.new(|_| Settings::load());
        let store = WorkspaceStore::load_or_migrate();
        let active_id = store.active_id().to_string();
        let workspaces = cx.new(|_| store);
        let nav = cx.new(|_| NavModel::load_for(&active_id));
        let jobs = cx.new(|_| JobQueue::new());
        let clipboard = cx.new(|_| FileClipboard::default());
        let selection = cx.new(|_| SelectionCtx::default());
        let drive_stats = cx.new(|_| DriveStatsStore::load());
        let thumbnails = cx.new(|_| ThumbnailCache::default());
        let preview_cache = cx.new(|_| PreviewCache::default());
        let audio = cx.new(|_| AudioPlayer::new());
        let git = cx.new(|_| GitStore::new());
        cx.set_global(Self {
            backend,
            settings,
            workspaces,
            nav,
            jobs,
            clipboard,
            selection,
            drive_stats,
            thumbnails,
            preview_cache,
            audio,
            git,
            active_explorer: RefCell::new(None),
            nav_panel: RefCell::new(None),
            inspector: RefCell::new(None),
        });
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// The asynchronous filesystem backend.
    pub fn backend(&self) -> &crate::backend::Backend {
        &self.backend
    }

    pub fn set_active_explorer(&self, panel: WeakEntity<ExplorerPanel>) {
        *self.active_explorer.borrow_mut() = Some(panel);
    }

    pub fn active_explorer(&self) -> Option<WeakEntity<ExplorerPanel>> {
        self.active_explorer.borrow().clone()
    }

    pub fn set_nav_panel(&self, panel: WeakEntity<NavPanel>) {
        *self.nav_panel.borrow_mut() = Some(panel);
    }

    pub fn nav_panel(&self) -> Option<WeakEntity<NavPanel>> {
        self.nav_panel.borrow().clone()
    }

    pub fn set_inspector(&self, panel: WeakEntity<InspectorPanel>) {
        *self.inspector.borrow_mut() = Some(panel);
    }

    pub fn inspector(&self) -> Option<WeakEntity<InspectorPanel>> {
        self.inspector.borrow().clone()
    }
}
