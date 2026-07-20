//! The serializable part of an explorer pane session. Restored through the
//! dock layout's `PanelInfo` payload.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ViewMode {
    List,
    Grid,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortBy {
    Name,
    Size,
    Modified,
    Type,
}

impl SortBy {
    #[allow(dead_code)]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Size => "Size",
            Self::Modified => "Modified",
            Self::Type => "Type",
        }
    }
}

/// Zoom bounds for the file views; the level multiplies row/tile geometry.
pub const ZOOM_MIN: f32 = 0.7;
pub const ZOOM_MAX: f32 = 2.0;
pub const ZOOM_STEP: f32 = 0.1;

/// Maximum navigation-history entries persisted per direction.
pub const MAX_HISTORY: usize = 25;

fn default_zoom() -> f32 {
    1.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneSession {
    pub cwd: PathBuf,
    pub view: ViewMode,
    pub sort: SortBy,
    pub ascending: bool,
    // `default` keeps layouts saved before these fields existed loading fine.
    #[serde(default = "default_zoom")]
    pub zoom: f32,
    #[serde(default)]
    pub back_stack: Vec<PathBuf>,
    #[serde(default)]
    pub fwd_stack: Vec<PathBuf>,
}

impl PaneSession {
    pub fn at(cwd: PathBuf) -> Self {
        Self {
            cwd,
            view: ViewMode::List,
            sort: SortBy::Name,
            ascending: true,
            zoom: 1.0,
            back_stack: Vec::new(),
            fwd_stack: Vec::new(),
        }
    }
}
