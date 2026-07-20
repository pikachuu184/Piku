pub mod pane_state;
pub mod persistence;
pub mod settings;
pub mod workspace;

use std::cell::RefCell;
use std::path::PathBuf;

use gpui::{App, AppContext as _, Entity, Global, WeakEntity};

use crate::core::entry::FsEntry;
use crate::services::jobs::JobQueue;
use crate::state::settings::Settings;
use crate::state::workspace::NavModel;
use crate::ui::explorer::ExplorerPanel;

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
    pub nav: Entity<NavModel>,
    pub jobs: Entity<JobQueue>,
    pub clipboard: Entity<FileClipboard>,
    pub selection: Entity<SelectionCtx>,
    active_explorer: RefCell<Option<WeakEntity<ExplorerPanel>>>,
}

impl Global for PikuState {}

impl PikuState {
    pub fn init(cx: &mut App) {
        let settings = cx.new(|_| Settings::load());
        let nav = cx.new(|_| NavModel::load());
        let jobs = cx.new(|_| JobQueue::new());
        let clipboard = cx.new(|_| FileClipboard::default());
        let selection = cx.new(|_| SelectionCtx::default());
        cx.set_global(Self {
            settings,
            nav,
            jobs,
            clipboard,
            selection,
            active_explorer: RefCell::new(None),
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
}
