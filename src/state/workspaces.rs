//! Named workspaces: the top-level unit of persisted state. Each workspace
//! owns a directory `workspaces/<id>/` in the app data dir holding its dock
//! layout and navigation model; `workspaces.json` indexes them and records
//! which one is active.
//!
//! Workspace state stores paths and layout only — never credentials.
//!
//! Migration: on first run after the upgrade, a flat `layout.json` /
//! `navigation.json` in the data dir is moved into a new "Default" workspace
//! (the old flat files are consumed).

use std::fs;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::state::persistence;

const INDEX_FILE: &str = "workspaces.json";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceMeta {
    pub id: String,
    pub name: String,
    pub created: DateTime<Utc>,
    pub last_opened: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct WorkspaceIndex {
    workspaces: Vec<WorkspaceMeta>,
    active_id: Option<String>,
}

pub struct WorkspaceStore {
    index: WorkspaceIndex,
    /// Returned by [`WorkspaceStore::active`] if the index is ever empty.
    ///
    /// `load_or_migrate` seeds a "Default" workspace and `delete` refuses to
    /// remove the last one, so this is unreachable in practice — but `active`
    /// hands out a `&WorkspaceMeta` and a corrupt state file must degrade to a
    /// usable app, not a panic on startup.
    fallback: WorkspaceMeta,
}

impl WorkspaceStore {
    /// Relative persistence path for a workspace's dock layout.
    pub fn layout_file(id: &str) -> String {
        format!("workspaces/{id}/layout.json")
    }

    /// Relative persistence path for a workspace's navigation model.
    pub fn nav_file(id: &str) -> String {
        format!("workspaces/{id}/navigation.json")
    }

    pub fn load_or_migrate() -> Self {
        let mut index: WorkspaceIndex = persistence::load_json(INDEX_FILE).unwrap_or_default();

        if index.workspaces.is_empty() {
            // First run (or wiped index): create the default workspace and
            // migrate any pre-workspace flat state files into it.
            let meta = new_meta("Default");
            let dir = persistence::state_path(&format!("workspaces/{}", meta.id));
            let _ = fs::create_dir_all(&dir);
            for file in ["layout.json", "navigation.json"] {
                let flat = persistence::state_path(file);
                if flat.exists() {
                    // Windows can refuse the rename with a sharing violation
                    // if another process still holds the file (AV scan, a
                    // previous instance mid-shutdown) — fall back to copy so
                    // the workspace still inherits the old state.
                    if fs::rename(&flat, dir.join(file)).is_err()
                        && fs::copy(&flat, dir.join(file)).is_ok()
                    {
                        let _ = fs::remove_file(&flat);
                    }
                }
            }
            index.active_id = Some(meta.id.clone());
            index.workspaces.push(meta);
        }

        // Heal a missing or dangling active id.
        let active_ok = index
            .active_id
            .as_ref()
            .is_some_and(|id| index.workspaces.iter().any(|w| &w.id == id));
        if !active_ok {
            index.active_id = index.workspaces.first().map(|w| w.id.clone());
        }

        let store = Self {
            fallback: new_meta("Default"),
            index,
        };
        store.save();
        store
    }

    pub fn list(&self) -> &[WorkspaceMeta] {
        &self.index.workspaces
    }

    pub fn active(&self) -> &WorkspaceMeta {
        let id = self.index.active_id.as_deref().unwrap_or_default();
        self.index
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .or_else(|| self.index.workspaces.first())
            .unwrap_or_else(|| {
                tracing::error!("workspace index is empty — falling back to a synthetic workspace");
                &self.fallback
            })
    }

    pub fn active_id(&self) -> &str {
        &self.active().id
    }

    pub fn set_active(&mut self, id: &str) {
        if let Some(meta) = self.index.workspaces.iter_mut().find(|w| w.id == id) {
            meta.last_opened = Utc::now();
            self.index.active_id = Some(id.to_string());
            self.save();
        }
    }

    /// Create a new empty workspace. Returns its id.
    pub fn create(&mut self, name: &str) -> Result<String, String> {
        let name = self.validated_name(name, None)?;
        let meta = new_meta(&name);
        let id = meta.id.clone();
        let _ = fs::create_dir_all(persistence::state_path(&format!("workspaces/{id}")));
        self.index.workspaces.push(meta);
        self.save();
        Ok(id)
    }

    pub fn rename(&mut self, id: &str, name: &str) -> Result<(), String> {
        let name = self.validated_name(name, Some(id))?;
        let Some(meta) = self.index.workspaces.iter_mut().find(|w| w.id == id) else {
            return Err("Workspace not found".into());
        };
        meta.name = name;
        self.save();
        Ok(())
    }

    /// Duplicate a workspace's persisted state into a new workspace.
    /// The caller must save the live layout first so the copy is current.
    pub fn duplicate(&mut self, id: &str) -> Result<String, String> {
        let source = self
            .index
            .workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| "Workspace not found".to_string())?;
        let copy_name = self.available_copy_name(&source.name.clone());
        let new_id = self.create(&copy_name)?;
        for file in ["layout.json", "navigation.json"] {
            let from = persistence::state_path(&format!("workspaces/{id}/{file}"));
            if from.exists() {
                let to = persistence::state_path(&format!("workspaces/{new_id}/{file}"));
                fs::copy(&from, &to).map_err(|error| error.to_string())?;
            }
        }
        Ok(new_id)
    }

    /// Delete a workspace and its state directory. Refuses to delete the
    /// last workspace or the active one (switch away first).
    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        if self.index.workspaces.len() <= 1 {
            return Err("Cannot delete the last workspace".into());
        }
        if self.active_id() == id {
            return Err("Cannot delete the active workspace".into());
        }
        let Some(ix) = self.index.workspaces.iter().position(|w| w.id == id) else {
            return Err("Workspace not found".into());
        };

        // Ids are self-generated, but never remove a directory based on a
        // string that could traverse out of the data dir.
        if id.is_empty() || id.contains(['/', '\\', ':']) || id.contains("..") {
            return Err("Invalid workspace id".into());
        }
        let dir = persistence::state_path(&format!("workspaces/{id}"));
        debug_assert!(dir.starts_with(persistence::data_dir()));
        if dir.exists() {
            fs::remove_dir_all(&dir).map_err(|error| error.to_string())?;
        }

        self.index.workspaces.remove(ix);
        self.save();
        Ok(())
    }

    /// The most recently opened workspace other than `except`, for picking a
    /// fallback before deleting the active workspace.
    pub fn most_recent_other(&self, except: &str) -> Option<&WorkspaceMeta> {
        self.index
            .workspaces
            .iter()
            .filter(|w| w.id != except)
            .max_by_key(|w| w.last_opened)
    }

    fn validated_name(&self, name: &str, ignore_id: Option<&str>) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Workspace name cannot be empty".into());
        }
        if name.chars().count() > 60 {
            return Err("Workspace names are limited to 60 characters".into());
        }
        let clash = self
            .index
            .workspaces
            .iter()
            .any(|w| Some(w.id.as_str()) != ignore_id && w.name.eq_ignore_ascii_case(name));
        if clash {
            return Err(format!("A workspace named “{name}” already exists"));
        }
        Ok(name.to_string())
    }

    fn available_copy_name(&self, base: &str) -> String {
        let wanted = format!("{base} copy");
        if self.validated_name(&wanted, None).is_ok() {
            return wanted;
        }
        for n in 2..1000 {
            let wanted = format!("{base} copy {n}");
            if self.validated_name(&wanted, None).is_ok() {
                return wanted;
            }
        }
        format!("{base} copy {}", Utc::now().timestamp())
    }

    fn save(&self) {
        if let Err(error) = persistence::save_json(INDEX_FILE, &self.index) {
            tracing::warn!("failed to save workspace index: {error:#}");
        }
    }
}

fn new_meta(name: &str) -> WorkspaceMeta {
    let now = Utc::now();
    WorkspaceMeta {
        id: format!("ws-{}", now.timestamp_millis()),
        name: name.to_string(),
        created: now,
        last_opened: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(names: &[&str]) -> WorkspaceStore {
        let workspaces = names
            .iter()
            .enumerate()
            .map(|(ix, name)| WorkspaceMeta {
                id: format!("ws-test-{ix}"),
                name: name.to_string(),
                created: Utc::now(),
                last_opened: Utc::now(),
            })
            .collect::<Vec<_>>();
        let active_id = workspaces.first().map(|w| w.id.clone());
        WorkspaceStore {
            fallback: new_meta("Default"),
            index: WorkspaceIndex {
                workspaces,
                active_id,
            },
        }
    }

    #[test]
    fn name_validation() {
        let store = store_with(&["Default"]);
        assert!(store.validated_name("Work", None).is_ok());
        assert!(store.validated_name("", None).is_err());
        assert!(store.validated_name("   ", None).is_err());
        assert!(store.validated_name("default", None).is_err());
        assert!(store.validated_name("Default", Some("ws-test-0")).is_ok());
        assert!(store.validated_name(&"x".repeat(61), None).is_err());
    }

    #[test]
    fn delete_guards() {
        let mut store = store_with(&["Default"]);
        assert!(store.delete("ws-test-0").is_err()); // last workspace

        let mut store = store_with(&["Default", "Work"]);
        assert!(store.delete("ws-test-0").is_err()); // active workspace
        assert!(store.delete("missing").is_err());
    }

    #[test]
    fn most_recent_other_skips_self() {
        let store = store_with(&["A", "B"]);
        assert_eq!(
            store.most_recent_other("ws-test-0").unwrap().id,
            "ws-test-1"
        );
        assert!(store_with(&["A"]).most_recent_other("ws-test-0").is_none());
    }
}
