//! Cross-pane navigation model: favorites, pinned folders, and recents.
//! Scoped to a workspace — each workspace persists its own copy under
//! `workspaces/<id>/navigation.json`.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use gpui::Context;
use serde::{Deserialize, Serialize};

use crate::state::workspaces::WorkspaceStore;

const MAX_RECENTS: usize = 12;

/// How a drive's capacity is presented in the workspace sidebar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriveDisplayMode {
    #[default]
    Ring,
    Bar,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NavModel {
    /// Persistence target (relative to the data dir); set by `load_for`,
    /// never serialized.
    #[serde(skip)]
    file: String,
    pub favorites: Vec<PathBuf>,
    pub pinned: Vec<PathBuf>,
    pub recents: VecDeque<PathBuf>,
    /// Sidebar sections the user has collapsed — part of the workspace, so
    /// each workspace restores its own sidebar shape.
    pub collapsed_sections: Vec<String>,
    /// Per-drive sidebar presentation, keyed by mount path and scoped to this
    /// workspace. Missing entries retain the default ring presentation.
    #[serde(default)]
    pub drive_display_modes: BTreeMap<PathBuf, DriveDisplayMode>,
}

impl NavModel {
    /// Load the navigation model belonging to the given workspace. Restored
    /// paths are re-authorized through the path guard so a hand-edited
    /// navigation.json cannot seed out-of-root paths into the UI model.
    pub fn load_for(workspace_id: &str) -> Self {
        let file = WorkspaceStore::nav_file(workspace_id);
        let mut model: Self = crate::state::persistence::load_json(&file).unwrap_or_default();
        model.file = file;

        let guard = crate::storage::local();
        let guard = guard.guard();
        let before = model.favorites.len() + model.pinned.len() + model.recents.len();
        model.favorites.retain(|p| guard.sanitize(p).is_ok());
        model.pinned.retain(|p| guard.sanitize(p).is_ok());
        model.recents.retain(|p| guard.sanitize(p).is_ok());
        let dropped = before - (model.favorites.len() + model.pinned.len() + model.recents.len());
        if dropped > 0 {
            tracing::warn!("dropped {dropped} unauthorized path(s) restoring navigation state");
        }
        model
    }

    pub fn save(&self) {
        if let Err(error) = crate::state::persistence::save_json(&self.file, self) {
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

    pub fn is_section_collapsed(&self, id: &str) -> bool {
        self.collapsed_sections.iter().any(|s| s == id)
    }

    pub fn drive_display(&self, mount: &Path) -> DriveDisplayMode {
        self.drive_display_modes
            .get(mount)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_drive_display(
        &mut self,
        mount: PathBuf,
        mode: DriveDisplayMode,
        cx: &mut Context<Self>,
    ) {
        if self.drive_display(&mount) == mode {
            return;
        }
        self.drive_display_modes.insert(mount, mode);
        self.save();
        cx.notify();
    }

    pub fn toggle_section(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self.collapsed_sections.iter().position(|s| s == id) {
            self.collapsed_sections.remove(ix);
        } else {
            self.collapsed_sections.push(id.to_string());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_navigation_state_defaults_drive_display_to_ring() {
        let model: NavModel = serde_json::from_str("{}").expect("default navigation state");
        assert_eq!(
            model.drive_display(Path::new("/mnt/data")),
            DriveDisplayMode::Ring
        );
    }

    #[test]
    fn drive_display_choices_round_trip_by_mount_path() {
        let mut model = NavModel::default();
        model
            .drive_display_modes
            .insert(PathBuf::from("/mnt/data"), DriveDisplayMode::Bar);

        let encoded = serde_json::to_string(&model).expect("serialize navigation state");
        let restored: NavModel = serde_json::from_str(&encoded).expect("restore navigation state");

        assert_eq!(
            restored.drive_display(Path::new("/mnt/data")),
            DriveDisplayMode::Bar
        );
        assert_eq!(
            restored.drive_display(Path::new("/mnt/other")),
            DriveDisplayMode::Ring
        );
    }
}
