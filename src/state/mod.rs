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
use crate::services::jobs::JobQueue;
use crate::services::preview_cache::PreviewCache;
use crate::services::thumbnails::ThumbnailCache;
use crate::state::nav_model::NavModel;
use crate::state::settings::Settings;
use crate::state::workspaces::WorkspaceStore;
use crate::ui::explorer::ExplorerPanel;
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
    active_explorer: RefCell<Option<WeakEntity<ExplorerPanel>>>,
    nav_panel: RefCell<Option<WeakEntity<NavPanel>>>,
}

impl Global for PikuState {}

impl PikuState {
    pub fn init(cx: &mut App) {
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
        cx.set_global(Self {
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
            active_explorer: RefCell::new(None),
            nav_panel: RefCell::new(None),
        });
    }

    pub fn global(cx: &App) -> &Self {
        cx.global::<Self>()
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
}
