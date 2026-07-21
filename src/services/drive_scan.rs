//! Background drive scanner: aggregates used bytes per file category so the
//! sidebar can draw segmented capacity bars. Maintenance work, deliberately
//! not a `JobQueue` job — queue completions raise toasts and status-bar
//! progress, and this must stay silent.
//!
//! Safety posture: read-only traversal, symlinks and every Windows reparse
//! point (junctions, mount points, OneDrive placeholders) are never entered,
//! all per-entry errors are swallowed, and both depth and queue size are
//! bounded so a pathological tree cannot exhaust memory.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::Context;
use serde::{Deserialize, Serialize};

use crate::core::file_type::{FileCategory, categorize_ext};

/// Cache file under the piku data dir.
const CACHE_FILE: &str = "drive_stats.json";
/// Rescan a drive when its cached stats are older than this.
const STALE_AFTER_SECS: u64 = 24 * 60 * 60;
/// Traversal caps: beyond either, the scan reports `complete: false`.
const MAX_DEPTH: u32 = 12;
const MAX_QUEUED_DIRS: usize = 100_000;
/// Coalesce streamed updates so the UI repaints at a sane rate.
const UPDATE_EVERY: Duration = Duration::from_millis(250);

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// The categories a scan reports, in the order the bar renders them.
pub const SCAN_CATEGORIES: [FileCategory; 8] = [
    FileCategory::Image,
    FileCategory::Video,
    FileCategory::Audio,
    FileCategory::Archive,
    FileCategory::Document,
    FileCategory::Code,
    FileCategory::Executable,
    FileCategory::Other,
];

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct DriveStats {
    /// Bytes per category, keyed by `FileCategory::label()`.
    pub by_category: HashMap<String, u64>,
    /// Unix seconds when the scan finished (0 while still running).
    pub scanned_at: u64,
    /// False when a traversal cap was hit or the scan is still running.
    pub complete: bool,
}

impl DriveStats {
    pub fn bytes_for(&self, category: FileCategory) -> u64 {
        self.by_category.get(category.label()).copied().unwrap_or(0)
    }
}

enum ScanUpdate {
    /// Streaming partial totals for the drive currently being scanned.
    Partial { mount: PathBuf, stats: DriveStats },
    /// Final totals for one drive; the cache is persisted at this point.
    Complete { mount: PathBuf, stats: DriveStats },
}

pub struct DriveStatsStore {
    stats: HashMap<PathBuf, DriveStats>,
    cancel: Arc<AtomicBool>,
    scanning: bool,
}

impl DriveStatsStore {
    pub fn load() -> Self {
        Self {
            stats: crate::state::persistence::load_json(CACHE_FILE).unwrap_or_default(),
            cancel: Arc::new(AtomicBool::new(false)),
            scanning: false,
        }
    }

    pub fn get(&self, mount: &Path) -> Option<&DriveStats> {
        self.stats.get(mount)
    }

    /// Kick off one sequential background worker over every stale drive.
    /// No-op when a worker is already running or nothing is stale.
    pub fn ensure_scans(&mut self, mounts: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.scanning {
            return;
        }
        let now = unix_now();
        let stale: Vec<PathBuf> = mounts
            .into_iter()
            .filter(|mount| {
                self.stats
                    .get(mount)
                    .map(|s| !s.complete || now.saturating_sub(s.scanned_at) > STALE_AFTER_SECS)
                    .unwrap_or(true)
            })
            .collect();
        if stale.is_empty() {
            return;
        }

        self.scanning = true;
        let cancel = self.cancel.clone();
        let (tx, mut rx) = unbounded::<ScanUpdate>();

        cx.background_executor()
            .spawn(async move {
                for mount in stale {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    scan_drive(&mount, &cancel, &tx);
                }
            })
            .detach();

        cx.spawn(async move |this, cx| {
            while let Some(update) = rx.next().await {
                let stop = this
                    .update(cx, |store: &mut Self, cx| {
                        match update {
                            ScanUpdate::Partial { mount, stats } => {
                                store.stats.insert(mount, stats);
                            }
                            ScanUpdate::Complete { mount, stats } => {
                                store.stats.insert(mount, stats);
                                let _ = crate::state::persistence::save_json(
                                    CACHE_FILE,
                                    &store.stats,
                                );
                            }
                        }
                        cx.notify();
                    })
                    .is_err();
                if stop {
                    return;
                }
            }
            let _ = this.update(cx, |store: &mut Self, cx| {
                store.scanning = false;
                cx.notify();
            });
        })
        .detach();
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Iterative BFS over one drive (runs on the background executor).
fn scan_drive(root: &Path, cancel: &AtomicBool, tx: &UnboundedSender<ScanUpdate>) {
    // Read-only, but the root still goes through the guard for consistency.
    let Ok(root) = crate::storage::local().guard().sanitize(root) else {
        return;
    };

    let mut bytes = [0u64; SCAN_CATEGORIES.len()];
    let mut complete = true;
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::from([(root.clone(), 0)]);
    let mut last_update = Instant::now();

    while let Some((dir, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue; // access denied, vanished — skip silently
        };
        for child in children.flatten() {
            let path = child.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // Never enter links or reparse points (junctions, mount points,
            // cloud placeholders) — no cycles, no double counting.
            if md.file_type().is_symlink() || is_reparse_point(&md) {
                continue;
            }
            if md.is_dir() {
                if depth + 1 > MAX_DEPTH || queue.len() >= MAX_QUEUED_DIRS {
                    complete = false;
                    continue;
                }
                queue.push_back((path, depth + 1));
            } else if md.is_file() {
                let ext = path
                    .extension()
                    .map(|e| e.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                let category = categorize_ext(&ext);
                if let Some(slot) = SCAN_CATEGORIES.iter().position(|c| *c == category) {
                    bytes[slot] += md.len();
                }
            }
        }

        if last_update.elapsed() >= UPDATE_EVERY {
            last_update = Instant::now();
            let _ = tx.unbounded_send(ScanUpdate::Partial {
                mount: root.clone(),
                stats: snapshot(&bytes, 0, false),
            });
        }
    }

    let _ = tx.unbounded_send(ScanUpdate::Complete {
        mount: root,
        stats: snapshot(&bytes, unix_now(), complete),
    });
}

#[cfg(windows)]
fn is_reparse_point(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

fn snapshot(bytes: &[u64; SCAN_CATEGORIES.len()], scanned_at: u64, complete: bool) -> DriveStats {
    DriveStats {
        by_category: SCAN_CATEGORIES
            .iter()
            .zip(bytes.iter())
            .filter(|(_, b)| **b > 0)
            .map(|(c, b)| (c.label().to_string(), *b))
            .collect(),
        scanned_at,
        complete,
    }
}
