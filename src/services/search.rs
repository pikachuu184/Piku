//! Per-tab recursive file search: a background BFS that streams filename
//! matches back to the owning pane. Each explorer pane runs its own search, so
//! searches in different tabs are fully isolated and continue independently.
//!
//! Safety posture mirrors the drive scanner (`drive_scan`): read-only, the
//! guard authorizes the root, symlinks and every Windows reparse point
//! (junctions, mount points, cloud placeholders) are never entered, all
//! per-entry errors are swallowed, and depth / visited-dir / hit caps bound the
//! work so a pathological tree cannot hang the search or exhaust memory.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use gpui::BackgroundExecutor;

use crate::core::entry::FsEntry;

/// Traversal caps: past any of these the search stops early and reports the
/// results it has (`complete: false`).
const MAX_DEPTH: u32 = 16;
const MAX_VISITED_DIRS: usize = 200_000;
const MAX_HITS: usize = 5_000;
/// Coalesce streamed batches so the UI repaints at a sane rate.
const FLUSH_EVERY: Duration = Duration::from_millis(150);

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// A streamed message from the worker: a batch of new matches, or the terminal
/// marker once the traversal ends.
pub enum SearchUpdate {
    Batch(Vec<FsEntry>),
    Done { complete: bool },
}

/// Start a background recursive search under `root` for names containing
/// `query` (case-insensitive). Returns the receiver the pane drains; flip
/// `cancel` to stop the worker early (the pane does this when the query changes
/// or it navigates away).
pub fn start(
    root: PathBuf,
    query: String,
    cancel: Arc<AtomicBool>,
    executor: BackgroundExecutor,
) -> UnboundedReceiver<SearchUpdate> {
    let (tx, rx) = unbounded::<SearchUpdate>();
    executor
        .spawn(async move {
            run(&root, &query, &cancel, &tx);
        })
        .detach();
    rx
}

/// Iterative BFS over the tree (runs on the background executor).
fn run(root: &Path, query: &str, cancel: &AtomicBool, tx: &UnboundedSender<SearchUpdate>) {
    // Read-only, but the root still goes through the guard for consistency and
    // to normalize verbatim prefixes before traversal.
    let Ok(root) = crate::storage::local().guard().sanitize(root) else {
        let _ = tx.unbounded_send(SearchUpdate::Done { complete: true });
        return;
    };

    let needle = query.to_lowercase();
    let mut complete = true;
    let mut visited = 0usize;
    let mut hits = 0usize;
    let mut queue: VecDeque<(PathBuf, u32)> = VecDeque::from([(root, 0)]);
    let mut batch: Vec<FsEntry> = Vec::new();
    let mut last_flush = Instant::now();

    'outer: while let Some((dir, depth)) = queue.pop_front() {
        if cancel.load(Ordering::Relaxed) {
            return; // superseded — the pane ignores late results anyway
        }
        visited += 1;
        let Ok(children) = std::fs::read_dir(&dir) else {
            continue; // access denied, vanished — skip silently
        };
        for child in children.flatten() {
            let path = child.path();
            let Ok(md) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            // Never follow links or reparse points — no cycles, no escaping the
            // authorized subtree through a junction.
            if md.file_type().is_symlink() || is_reparse_point(&md) {
                continue;
            }

            let matched = path
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase().contains(&needle))
                .unwrap_or(false);
            if matched {
                batch.push(FsEntry::from_metadata(path.clone(), &md));
                hits += 1;
                if hits >= MAX_HITS {
                    complete = false;
                    break 'outer;
                }
            }

            if md.is_dir() {
                if depth + 1 > MAX_DEPTH || visited >= MAX_VISITED_DIRS {
                    complete = false;
                    continue;
                }
                queue.push_back((path, depth + 1));
            }
        }

        if last_flush.elapsed() >= FLUSH_EVERY && !batch.is_empty() {
            last_flush = Instant::now();
            let _ = tx.unbounded_send(SearchUpdate::Batch(std::mem::take(&mut batch)));
        }
    }

    if !batch.is_empty() {
        let _ = tx.unbounded_send(SearchUpdate::Batch(batch));
    }
    let _ = tx.unbounded_send(SearchUpdate::Done { complete });
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
