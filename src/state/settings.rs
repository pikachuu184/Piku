//! Application settings, persisted as JSON.

use serde::{Deserialize, Serialize};

use crate::state::pane_state::{SortBy, ViewMode};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub show_hidden: bool,
    pub default_view: ViewMode,
    pub default_sort: SortBy,
    pub confirm_delete: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_hidden: false,
            default_view: ViewMode::List,
            default_sort: SortBy::Name,
            confirm_delete: true,
        }
    }
}

const FILE: &str = "settings.json";

impl Settings {
    pub fn load() -> Self {
        crate::state::persistence::load_json(FILE).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Err(error) = crate::state::persistence::save_json(FILE, self) {
            tracing::warn!("failed to save settings: {error:#}");
        }
    }
}
