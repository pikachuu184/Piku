//! The transfer engine. Jobs run on the background executor and stream
//! progress events back to the UI thread; the render loop never blocks.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{Context, Window};
use gpui_component::WindowExt as _;

use crate::security::file_name::validate_name;
use crate::security::path_guard::PathGuard;
use crate::services::jobs::label;
use crate::services::jobs::{Job, JobEvent, JobKind, JobStatus};
use crate::storage::provider::StorageProvider;

const KEEP_FINISHED_JOBS: usize = 20;
/// Coalesce byte progress so the UI is not notified per copy chunk.
const PROGRESS_GRANULARITY: u64 = 4 * 1024 * 1024;

pub struct JobQueue {
    jobs: Vec<Job>,
    next_id: u64,
}

impl JobQueue {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            next_id: 1,
        }
    }

    pub fn active_job(&self) -> Option<&Job> {
        self.jobs.iter().rev().find(|job| job.is_active())
    }

    #[allow(dead_code)]
    pub fn active_count(&self) -> usize {
        self.jobs.iter().filter(|job| job.is_active()).count()
    }

    pub fn cancel_active(&mut self, cx: &mut Context<Self>) {
        for job in self.jobs.iter().filter(|job| job.is_active()) {
            job.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    pub fn submit_copy(
        &mut self,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        is_move: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if sources.is_empty() {
            return;
        }
        let kind = if is_move {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        let title = label::transfer_title(kind.label(), &sources[0], sources.len());
        let provider = crate::storage::local_dyn();
        let guard = crate::storage::local().guard().clone();
        self.spawn_job(kind, title, window, cx, move |tx, cancel| {
            copy_work(provider, guard, sources, dest_dir, is_move, tx, cancel)
        });
    }

    pub fn submit_delete(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let title = label::delete_title(&paths[0], paths.len());
        let provider = crate::storage::local_dyn();
        self.spawn_job(JobKind::Delete, title, window, cx, move |tx, _cancel| {
            let count = paths.len();
            let _ = tx.unbounded_send(JobEvent::Scanned {
                total_bytes: 0,
                total_items: count,
            });
            let result = provider
                .delete_to_trash(&paths)
                .map(|_| label::delete_summary(count))
                .map_err(|error| error.to_string());
            let _ = tx.unbounded_send(JobEvent::Progress {
                delta_bytes: 0,
                delta_items: count,
            });
            let _ = tx.unbounded_send(JobEvent::Finished(result));
        });
    }

    pub fn submit_rename(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Defense in depth: dialogs validate too, but every rename that
        // reaches the engine gets checked regardless of caller.
        if let Some(name) = to.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::rename_title(&from);
        let provider = crate::storage::local_dyn();
        self.spawn_job(JobKind::Rename, title, window, cx, move |tx, _cancel| {
            let result = provider
                .rename(&from, &to)
                .map(|_| label::rename_summary(&to))
                .map_err(|error| error.to_string());
            let _ = tx.unbounded_send(JobEvent::Finished(result));
        });
    }

    pub fn submit_new_folder(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::create_title(&path);
        let provider = crate::storage::local_dyn();
        self.spawn_job(JobKind::NewFolder, title, window, cx, move |tx, _cancel| {
            let result = provider
                .create_dir(&path)
                .map(|_| label::create_summary(&path))
                .map_err(|error| error.to_string());
            let _ = tx.unbounded_send(JobEvent::Finished(result));
        });
    }

    pub fn submit_new_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::create_title(&path);
        let provider = crate::storage::local_dyn();
        self.spawn_job(JobKind::NewFile, title, window, cx, move |tx, _cancel| {
            let result = provider
                .create_file(&path)
                .map(|_| label::create_summary(&path))
                .map_err(|error| error.to_string());
            let _ = tx.unbounded_send(JobEvent::Finished(result));
        });
    }

    /// Fetch one remote of a repository as a background job (progress and
    /// cancel share the transfer engine's UI). Completion writes an audit
    /// record and asks the `GitStore` to refresh the repository.
    pub fn submit_git_fetch(
        &mut self,
        root: PathBuf,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = label::fetch_title(&remote);
        let backend = crate::state::PikuState::global(cx).git.read(cx).backend();
        let job_root = root;
        self.spawn_job(JobKind::GitFetch, title, window, cx, move |tx, cancel| {
            use crate::services::git::backend::GitBackend as _;
            let result = backend.fetch(&job_root, &remote, &tx, &cancel);
            let (ok, message) = match &result {
                Ok(outcome) => (
                    true,
                    label::fetch_summary(&outcome.remote, outcome.updated_refs),
                ),
                Err(error) => (false, error.to_string()),
            };
            crate::security::audit::record("git.fetch", &job_root, None, ok, &remote);
            let _ = tx.unbounded_send(JobEvent::Finished(if ok {
                Ok(message)
            } else if cancel.load(Ordering::Relaxed) {
                Err(label::CANCELLED.into())
            } else {
                Err(message)
            }));
        });
        // No explicit refresh needed: a fetch that updated anything touched
        // `.git`/`.git/refs/remotes`, which the repo watchers already cover.
    }

    fn spawn_job(
        &mut self,
        kind: JobKind,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(UnboundedSender<JobEvent>, Arc<AtomicBool>) + Send + 'static,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let job = Job::new(id, kind, title);
        let cancel = job.cancel.clone();
        self.jobs.push(job);
        self.trim(cx);
        cx.notify();

        let (tx, mut rx) = unbounded::<JobEvent>();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    work(tx, cancel);
                })
                .detach();

            while let Some(event) = rx.next().await {
                let finished = matches!(event, JobEvent::Finished(_));
                if this
                    .update_in(cx, |queue, window, cx| {
                        queue.apply_event(id, event, window, cx);
                    })
                    .is_err()
                    || finished
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_event(
        &mut self,
        id: u64,
        event: JobEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        match event {
            JobEvent::Scanned {
                total_bytes,
                total_items,
            } => {
                job.total_bytes = total_bytes;
                job.total_items = total_items;
            }
            JobEvent::Progress {
                delta_bytes,
                delta_items,
            } => {
                job.done_bytes += delta_bytes;
                job.done_items += delta_items;
            }
            JobEvent::Finished(result) => match result {
                Ok(message) => {
                    job.status = JobStatus::Done;
                    window.push_notification(crate::ui::toast::success(message), cx);
                }
                Err(error) => {
                    if label::is_cancelled(&error) {
                        job.status = JobStatus::Cancelled;
                        window
                            .push_notification(crate::ui::toast::info(label::CANCELLED_TOAST), cx);
                    } else {
                        job.status = JobStatus::Failed(error.clone());
                        window.push_notification(crate::ui::toast::error(error), cx);
                    }
                }
            },
        }
        cx.notify();
    }

    fn trim(&mut self, _cx: &mut Context<Self>) {
        let finished = self.jobs.iter().filter(|job| !job.is_active()).count();
        if finished > KEEP_FINISHED_JOBS {
            let mut to_remove = finished - KEEP_FINISHED_JOBS;
            self.jobs.retain(|job| {
                if to_remove > 0 && !job.is_active() {
                    to_remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Worker implementations (run on the background executor).
// ---------------------------------------------------------------------------

fn copy_work(
    provider: Arc<dyn StorageProvider>,
    guard: PathGuard,
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
    is_move: bool,
    tx: UnboundedSender<JobEvent>,
    cancel: Arc<AtomicBool>,
) {
    // Authorize everything up front; the engine below never touches a path
    // that has not passed the guard.
    let dest_dir = match guard.sanitize(&dest_dir) {
        Ok(path) => path,
        Err(error) => {
            let _ = tx.unbounded_send(JobEvent::Finished(Err(error.to_string())));
            return;
        }
    };
    let sources: Vec<PathBuf> = {
        let mut sanitized = Vec::with_capacity(sources.len());
        for source in &sources {
            match guard.sanitize(source) {
                Ok(path) => sanitized.push(path),
                Err(error) => {
                    let _ = tx.unbounded_send(JobEvent::Finished(Err(error.to_string())));
                    return;
                }
            }
        }
        sanitized
    };

    // Refuse to copy a directory into itself or its own subtree. Runs on the
    // sanitized (normalized) paths so `dest\..\src` aliases cannot slip past.
    for source in &sources {
        if dest_dir.starts_with(source) {
            let _ = tx.unbounded_send(JobEvent::Finished(Err(label::into_itself(is_move, source))));
            return;
        }
    }

    // Scan pass for totals.
    let mut total_bytes = 0u64;
    let mut total_items = 0usize;
    for source in &sources {
        if scan(&guard, source, &mut total_bytes, &mut total_items, &cancel).is_err() {
            let _ = tx.unbounded_send(JobEvent::Finished(Err(label::CANCELLED.into())));
            return;
        }
    }
    let _ = tx.unbounded_send(JobEvent::Scanned {
        total_bytes,
        total_items,
    });

    let mut moved_fast = 0usize;
    let mut copied = 0usize;
    let mut result: Result<(), String> = Ok(());

    for source in &sources {
        if cancel.load(Ordering::Relaxed) {
            result = Err(label::CANCELLED.into());
            break;
        }
        let Some(name) = source.file_name() else {
            continue;
        };
        let target = unique_destination(&guard, &dest_dir.join(name));

        if is_move && same_volume(source, &dest_dir) {
            // Stat before the rename — afterwards the source no longer exists.
            let size = std::fs::symlink_metadata(source)
                .map(|m| m.len())
                .unwrap_or(0);
            match provider.rename(source, &target) {
                Ok(()) => {
                    moved_fast += 1;
                    let _ = tx.unbounded_send(JobEvent::Progress {
                        delta_bytes: size,
                        delta_items: 1,
                    });
                    continue;
                }
                Err(_) => {
                    // Fall back to copy + remove below.
                }
            }
        }

        match copy_recursive(provider.as_ref(), &guard, source, &target, &tx, &cancel) {
            Ok(items) => {
                copied += items;
                if is_move && let Err(error) = remove_recursive(provider.as_ref(), &guard, source) {
                    result = Err(label::source_cleanup_failed(source, &error));
                    break;
                }
            }
            Err(error) => {
                result = Err(error);
                break;
            }
        }
    }

    let summary = label::transfer_summary(is_move, moved_fast + copied);
    let _ = tx.unbounded_send(JobEvent::Finished(result.map(|_| summary)));
}

fn scan(
    guard: &PathGuard,
    path: &Path,
    bytes: &mut u64,
    items: &mut usize,
    cancel: &AtomicBool,
) -> Result<(), ()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(());
    }
    // Unauthorized nodes are skipped without counting — the copy pass skips
    // them the same way, so totals stay honest.
    if guard.sanitize(path).is_err() {
        return Ok(());
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    *items += 1;
    if metadata.is_dir() && !metadata.is_symlink() {
        if let Ok(children) = std::fs::read_dir(path) {
            for child in children.flatten() {
                scan(guard, &child.path(), bytes, items, cancel)?;
            }
        }
    } else if metadata.is_file() {
        *bytes += metadata.len();
    }
    Ok(())
}

/// Copy a file or directory tree. Symlinks are never traversed or copied, and
/// every node is re-authorized through the guard before it is touched —
/// unauthorized children are skipped, mirroring the scan pass.
fn copy_recursive(
    provider: &dyn StorageProvider,
    guard: &PathGuard,
    source: &Path,
    target: &Path,
    tx: &UnboundedSender<JobEvent>,
    cancel: &Arc<AtomicBool>,
) -> Result<usize, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(label::CANCELLED.into());
    }
    if guard.sanitize(source).is_err() || guard.sanitize(target).is_err() {
        return Ok(0);
    }
    let metadata = std::fs::symlink_metadata(source)
        .map_err(|error| format!("{}: {error}", source.display()))?;

    if metadata.is_symlink() {
        // Skipped by policy; counts as a completed item so totals stay honest.
        let _ = tx.unbounded_send(JobEvent::Progress {
            delta_bytes: 0,
            delta_items: 1,
        });
        return Ok(0);
    }

    if metadata.is_dir() {
        // Routed through the provider (not raw create_dir_all) so directory
        // creation gains the same sanitize + audit trail as every mutation.
        provider
            .create_dir(target)
            .map_err(|error| format!("creating {}: {error}", target.display()))?;
        let _ = tx.unbounded_send(JobEvent::Progress {
            delta_bytes: 0,
            delta_items: 1,
        });
        let mut items = 1;
        let children =
            std::fs::read_dir(source).map_err(|error| format!("{}: {error}", source.display()))?;
        for child in children.flatten() {
            let child_source = child.path();
            let Some(name) = child_source.file_name() else {
                continue;
            };
            items += copy_recursive(
                provider,
                guard,
                &child_source,
                &target.join(name),
                tx,
                cancel,
            )?;
        }
        return Ok(items);
    }

    // Regular file: chunked copy with coalesced progress.
    let mut pending = 0u64;
    let mut report = |chunk: u64| {
        pending += chunk;
        if pending >= PROGRESS_GRANULARITY {
            let _ = tx.unbounded_send(JobEvent::Progress {
                delta_bytes: pending,
                delta_items: 0,
            });
            pending = 0;
        }
    };
    provider
        .copy_file(source, target, &mut report, cancel)
        .map_err(|error| error.to_string())?;
    let _ = tx.unbounded_send(JobEvent::Progress {
        delta_bytes: pending,
        delta_items: 1,
    });
    Ok(1)
}

/// Bottom-up removal of a source tree after a verified move-copy. Every node
/// is re-authorized before the traversal descends into it.
fn remove_recursive(
    provider: &dyn StorageProvider,
    guard: &PathGuard,
    path: &Path,
) -> anyhow::Result<()> {
    guard
        .sanitize(path)
        .map_err(|error| anyhow::anyhow!("{}: {error}", path.display()))?;
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.is_symlink() {
        // Containment check before descending. The lexical guard above cannot
        // see through a link: an attacker who swaps a child directory for a
        // symlink between our `symlink_metadata` and the `read_dir` would have
        // us recurse outside the subtree being removed. Comparing the child's
        // *resolved* path against the resolved root closes that in practice.
        //
        // Note this is mitigation, not proof: the resolve and the descent are
        // still two separate steps. Fully closing it needs `openat`-style
        // handles (`cap-std`), which is tracked separately.
        let root = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        for child in std::fs::read_dir(path)?.flatten() {
            let child_path = child.path();
            let child_meta = std::fs::symlink_metadata(&child_path)?;
            if child_meta.is_dir() && !child_meta.file_type().is_symlink() {
                let resolved =
                    dunce::canonicalize(&child_path).unwrap_or_else(|_| child_path.clone());
                if !resolved.starts_with(&root) {
                    anyhow::bail!(
                        "refusing to descend outside the deleted tree: {}",
                        child_path.display()
                    );
                }
            }
            remove_recursive(provider, guard, &child_path)?;
        }
    }
    provider.remove_after_move(path)
}

/// Explorer-style collision handling: `name.txt` → `name (2).txt`. The wanted
/// path is authorized before any filesystem probe; on failure it is returned
/// untouched so the caller's guarded operations reject it with a real error.
fn unique_destination(guard: &PathGuard, wanted: &Path) -> PathBuf {
    if guard.sanitize(wanted).is_err() {
        return wanted.to_path_buf();
    }
    if !wanted.exists() {
        return wanted.to_path_buf();
    }
    let parent = wanted.parent().unwrap_or_else(|| Path::new(""));
    let stem = wanted
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = wanted
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    for n in 2..10_000 {
        let candidate = if ext.is_empty() {
            parent.join(format!("{stem} ({n})"))
        } else {
            parent.join(format!("{stem} ({n}).{ext}"))
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    wanted.to_path_buf()
}

fn same_volume(a: &Path, b: &Path) -> bool {
    fn volume(path: &Path) -> Option<String> {
        path.components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().to_lowercase())
    }
    volume(a).is_some() && volume(a) == volume(b)
}
