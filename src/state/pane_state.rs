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

/// The store backing a tab. `Local` today; `Cloud` is reserved as the seam for
/// a future MinIO/R2 `StorageProvider` — a cloud tab would select it and carry
/// a `connection_id`. Credentials never live in the session (see `connection_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum StorageKind {
    #[default]
    Local,
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

/// A stable, process-unique tab id. Combines a monotonic counter with the
/// wall clock so ids stay distinct across restarts (restored tabs keep the id
/// serialized in their session; only freshly created tabs mint a new one).
pub fn new_tab_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}-{n:x}")
}

/// The serializable state of one tab — the whole "workspace session" for that
/// tab. Everything here survives a restart via the dock layout's `PanelInfo`.
/// Every new field is `#[serde(default)]` so layouts written before it existed
/// still deserialize (the reason `DOCK_VERSION` need not change).
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

    // -- Intelligent-tab session state (all serde-default for back-compat) --
    /// Stable identity for this tab across restarts.
    #[serde(default = "new_tab_id")]
    pub id: String,
    /// User-chosen tab title; falls back to the folder name when `None`.
    #[serde(default)]
    pub title: Option<String>,
    /// Pinned tabs cannot be closed by accident and are meant to persist.
    #[serde(default)]
    pub pinned: bool,
    /// Which backend serves this tab. Drives the tab's storage-type icon.
    #[serde(default)]
    pub storage: StorageKind,
    /// Cloud seam: a connection-profile id only — NEVER credentials/tokens.
    #[serde(default)]
    pub connection_id: Option<String>,
    /// In-view quick-filter text (used when `search_deep` is off).
    #[serde(default)]
    pub filter: String,
    /// Recursive-search query (used when `search_deep` is on).
    #[serde(default)]
    pub search_query: String,
    /// Whether the text box searches subfolders (recursive) vs. filters in view.
    #[serde(default)]
    pub search_deep: bool,
    /// Selected paths, persisted by path (indices shift across reloads).
    #[serde(default)]
    pub selection: Vec<PathBuf>,
    /// First row to scroll into view on restore (best-effort).
    #[serde(default)]
    pub scroll_first: usize,
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
            id: new_tab_id(),
            title: None,
            pinned: false,
            storage: StorageKind::Local,
            connection_id: None,
            filter: String::new(),
            search_query: String::new(),
            search_deep: false,
            selection: Vec::new(),
            scroll_first: 0,
        }
    }

    /// A deep copy with a fresh identity — the basis for Duplicate Tab. Carries
    /// full state (cwd, history, sort/view/zoom, filter, search, selection) but
    /// is independent from the original; a duplicate never inherits pinning.
    pub fn duplicate(&self) -> Self {
        let mut copy = self.clone();
        copy.id = new_tab_id();
        copy.pinned = false;
        copy
    }
}
