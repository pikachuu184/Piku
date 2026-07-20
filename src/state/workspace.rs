//! Cross-pane navigation model: favorites, pinned folders, and recents.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use gpui::Context;
use serde::{Deserialize, Serialize};

const FILE: &str = "navigation.json";
const MAX_RECENTS: usize = 12;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NavModel {
    pub favorites: Vec<PathBuf>,
    pub pinned: Vec<PathBuf>,
    pub recents: VecDeque<PathBuf>,
}

impl NavModel {
    pub fn load() -> Self {
        crate::state::persistence::load_json(FILE).unwrap_or_default()
    }

    fn save(&self) {
        if let Err(error) = crate::state::persistence::save_json(FILE, self) {
            tracing::warn!("failed to save navigation state: {error:#}");
        }
    }

    pub fn is_favorite(&self, path: &Path) -> bool {
        self.favorites.iter().any(|p| p == path)
    }

    pub fn is_pinned(&self, path: &Path) -> bool {
        self.pinned.iter().any(|p| p == path)
    }

    pub fn toggle_favorite(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(ix) = self.favorites.iter().position(|p| *p == path) {
            self.favorites.remove(ix);
        } else {
            self.favorites.push(path);
        }
        self.save();
        cx.notify();
    }

    pub fn toggle_pinned(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if let Some(ix) = self.pinned.iter().position(|p| *p == path) {
            self.pinned.remove(ix);
        } else {
            self.pinned.push(path);
        }
        self.save();
        cx.notify();
    }

    pub fn push_recent(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.recents.retain(|p| *p != path);
        self.recents.push_front(path);
        self.recents.truncate(MAX_RECENTS);
        self.save();
        cx.notify();
    }

    pub fn remove_recent(&mut self, path: &Path, cx: &mut Context<Self>) {
        self.recents.retain(|p| p != path);
        self.save();
        cx.notify();
    }
}
