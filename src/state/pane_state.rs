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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneSession {
    pub cwd: PathBuf,
    pub view: ViewMode,
    pub sort: SortBy,
    pub ascending: bool,
}

impl PaneSession {
    pub fn at(cwd: PathBuf) -> Self {
        Self {
            cwd,
            view: ViewMode::List,
            sort: SortBy::Name,
            ascending: true,
        }
    }
}
