//! Central repository state store. Panes report the directories they visit
//! (`note_dir`); the store discovers repositories passively, watches their
//! `.git` metadata, and keeps sanitized snapshots the UI observes. All git
//! work runs on the background executor — this entity only holds results.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt as _;
use gpui::Context;

use crate::services::watcher::DirWatcher;

use super::GIT_DEBOUNCE_MS;
use super::backend::{GitBackend, GitError};
use super::gix_backend::GixBackend;
use super::types::{GitFileStatus, RepoSnapshot, StatusSnapshot};

/// Repositories with no interest for this long get evicted on the next sweep.
const EVICT_AFTER: Duration = Duration::from_secs(15 * 60);

struct RepoEntry {
    snapshot: RepoSnapshot,
    status: StatusSnapshot,
    refresh_gen: u64,
    refreshing: bool,
    /// Interrupts the in-flight refresh when a newer one supersedes it.
    interrupt: Arc<AtomicBool>,
    /// Non-recursive watchers on `.git`, `.git/refs/heads`, `.git/refs/remotes`.
    _watchers: Vec<DirWatcher>,
    /// Last time a pane expressed interest (for eviction).
    last_touch: std::time::Instant,
}

pub struct GitStore {
    backend: Arc<GixBackend>,
    repos: HashMap<PathBuf, RepoEntry>,
    /// dir → repo root; `None` records "known not a repo" (negative cache).
    dir_index: HashMap<PathBuf, Option<PathBuf>>,
    /// Directories with discovery currently in flight.
    pending: HashSet<PathBuf>,
    /// Last failed mutation, for the UI to surface (and clear).
    last_error: Option<String>,
    /// Bumped after every successful mutation so views holding derived
    /// caches (commit timelines) know to reload.
    epoch: u64,
}

impl GitStore {
    pub fn new() -> Self {
        Self {
            backend: Arc::new(GixBackend::default()),
            repos: HashMap::new(),
            dir_index: HashMap::new(),
            pending: HashSet::new(),
            last_error: None,
            epoch: 0,
        }
    }

    /// The last mutation error; cleared by the next successful mutation.
    pub fn error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// See the field doc; observers compare this to invalidate caches.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn backend(&self) -> Arc<GixBackend> {
        self.backend.clone()
    }

    // ---- Read accessors (render code) ------------------------------------

    /// The repository root containing `dir`, when known.
    pub fn root_for(&self, dir: &Path) -> Option<&Path> {
        match self.dir_index.get(dir) {
            Some(Some(root)) => Some(root.as_path()),
            _ => None,
        }
    }

    pub fn snapshot(&self, root: &Path) -> Option<&RepoSnapshot> {
        self.repos.get(root).map(|entry| &entry.snapshot)
    }

    /// The full per-file status map for one repo (cheap `Arc` clones).
    pub fn status_snapshot(&self, root: &Path) -> Option<StatusSnapshot> {
        self.repos.get(root).map(|entry| entry.status.clone())
    }

    /// Status of one file, looked up in the repo that contains `dir`.
    pub fn status_of(&self, dir: &Path, path: &Path) -> Option<GitFileStatus> {
        let root = self.root_for(dir)?;
        self.repos.get(root)?.status.by_path.get(path).copied()
    }

    /// Whether an entry (file or collapsed directory) is git-ignored.
    pub fn is_ignored(&self, dir: &Path, path: &Path) -> bool {
        let Some(root) = self.root_for(dir) else {
            return false;
        };
        self.repos
            .get(root)
            .is_some_and(|entry| entry.status.ignored.contains(path))
    }

    /// Whether a directory contains at least one dirty entry.
    pub fn dir_dirty(&self, dir: &Path, path: &Path) -> bool {
        let Some(root) = self.root_for(dir) else {
            return false;
        };
        self.repos
            .get(root)
            .is_some_and(|entry| entry.status.dirty_dirs.contains(path))
    }

    // ---- Discovery --------------------------------------------------------

    /// Report that a pane is showing `dir`. Discovery runs at most once per
    /// directory (negative results cached); a hit registers the repo, starts
    /// its watchers, and kicks the first refresh.
    pub fn note_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        if self.pending.contains(&dir) {
            return;
        }
        if let Some(cached) = self.dir_index.get(&dir) {
            if let Some(root) = cached.clone() {
                self.touch(&root);
                // A repo evicted since the dir was indexed re-registers.
                if !self.repos.contains_key(&root) {
                    self.register_repo(root, cx);
                }
            }
            return;
        }
        self.pending.insert(dir.clone());
        self.sweep_stale();

        let backend = self.backend.clone();
        let probe = dir.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { backend.discover(&probe) })
                .await;
            let _ = this.update(cx, |store, cx| {
                store.pending.remove(&dir);
                store.dir_index.insert(dir, found.clone());
                if let Some(root) = found {
                    if !store.repos.contains_key(&root) {
                        store.register_repo(root, cx);
                    } else {
                        store.touch(&root);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn touch(&mut self, root: &Path) {
        if let Some(entry) = self.repos.get_mut(root) {
            entry.last_touch = std::time::Instant::now();
        }
    }

    /// Drop repos nobody has looked at recently; their watchers stop on drop.
    fn sweep_stale(&mut self) {
        let now = std::time::Instant::now();
        let stale: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.last_touch) > EVICT_AFTER)
            .map(|(root, _)| root.clone())
            .collect();
        for root in stale {
            if let Some(entry) = self.repos.remove(&root) {
                entry.interrupt.store(true, Ordering::Relaxed);
            }
            self.backend.evict(&root);
        }
    }

    fn register_repo(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let mut watchers = Vec::new();
        let git_dir = root.join(".git");
        // `.git` itself (HEAD, index, MERGE_HEAD…) plus the loose ref dirs.
        // All non-recursive; packed-refs and HEAD changes land in `.git`.
        for dir in [
            git_dir.clone(),
            git_dir.join("refs").join("heads"),
            git_dir.join("refs").join("remotes"),
        ] {
            if let Ok((watcher, mut rx)) = DirWatcher::watch(&dir) {
                watchers.push(watcher);
                let repo_root = root.clone();
                cx.spawn(async move |this, cx| {
                    while rx.next().await.is_some() {
                        // Debounce + drain: `.git` churn (index.lock cycles,
                        // ref updates) collapses into one refresh.
                        cx.background_executor()
                            .timer(Duration::from_millis(GIT_DEBOUNCE_MS))
                            .await;
                        while rx.try_recv().is_ok() {}
                        let alive = this
                            .update(cx, |store: &mut Self, cx| {
                                if store.repos.contains_key(&repo_root) {
                                    store.refresh(repo_root.clone(), cx);
                                    true
                                } else {
                                    false
                                }
                            })
                            .unwrap_or(false);
                        if !alive {
                            return;
                        }
                    }
                })
                .detach();
            }
        }

        self.repos.insert(
            root.clone(),
            RepoEntry {
                snapshot: RepoSnapshot {
                    root: root.clone(),
                    ..Default::default()
                },
                status: StatusSnapshot::default(),
                refresh_gen: 0,
                refreshing: false,
                interrupt: Arc::new(AtomicBool::new(false)),
                _watchers: watchers,
                last_touch: std::time::Instant::now(),
            },
        );
        self.refresh(root, cx);
    }

    // ---- Refresh -----------------------------------------------------------

    /// Recompute status + snapshot for one repo on the background executor.
    /// Generation-guarded: superseded refreshes are interrupted and their
    /// results discarded.
    pub fn refresh(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let Some(entry) = self.repos.get_mut(&root) else {
            return;
        };
        entry.refresh_gen += 1;
        let generation = entry.refresh_gen;
        // Supersede the in-flight run, if any.
        entry.interrupt.store(true, Ordering::Relaxed);
        let interrupt = Arc::new(AtomicBool::new(false));
        entry.interrupt = interrupt.clone();
        entry.refreshing = true;

        let backend = self.backend.clone();
        let work_root = root.clone();
        cx.spawn(async move |this, cx| {
            let worker_interrupt = interrupt.clone();
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = backend.status(&work_root, &worker_interrupt)?;
                    let mut snapshot = backend.snapshot(&work_root, &worker_interrupt)?;
                    // Merge cleanliness counts from the single status walk.
                    for file_status in status.by_path.values() {
                        use super::types::GitStatusCode as Code;
                        if file_status.index.is_some() {
                            snapshot.staged += 1;
                        }
                        match file_status.worktree {
                            Some(Code::Conflicted) => snapshot.conflicted += 1,
                            Some(Code::Untracked) => snapshot.untracked += 1,
                            Some(_) => snapshot.unstaged += 1,
                            None => {}
                        }
                    }
                    snapshot.truncated =
                        status.by_path.len() >= super::STATUS_MAX_ENTRIES;
                    Ok::<_, GitError>((snapshot, status))
                })
                .await;

            let _ = this.update(cx, |store, cx| {
                let Some(entry) = store.repos.get_mut(&root) else {
                    return;
                };
                if entry.refresh_gen != generation {
                    return; // superseded
                }
                entry.refreshing = false;
                match result {
                    Ok((snapshot, status)) => {
                        entry.snapshot = snapshot;
                        entry.status = status;
                    }
                    Err(GitError::Interrupted) => {}
                    Err(err) => {
                        // Repo may have vanished (`.git` deleted): drop it.
                        tracing::debug!(target: "piku::git", root = %root.display(), %err, "git refresh failed");
                        if !root.join(".git").is_dir() {
                            store.repos.remove(&root);
                            store.backend.evict(&root);
                            store
                                .dir_index
                                .retain(|_, cached| cached.as_deref() != Some(root.as_path()));
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ---- Mutations ---------------------------------------------------------

    /// Run a mutating backend operation on the background executor, write an
    /// audit record, refresh on success, and surface failures via
    /// `take_error`. Every mutating git operation funnels through here.
    fn run_mutation(
        &mut self,
        root: PathBuf,
        op: &'static str,
        detail: String,
        cx: &mut Context<Self>,
        work: impl FnOnce(Arc<GixBackend>) -> Result<String, GitError> + Send + 'static,
    ) {
        let backend = self.backend.clone();
        cx.spawn(async move |this, cx| {
            let audit_root = root.clone();
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let result = work(backend);
                    let (ok, note) = match &result {
                        Ok(note) => (true, note.clone()),
                        Err(err) => (false, err.to_string()),
                    };
                    crate::security::audit::record(
                        op,
                        &audit_root,
                        None,
                        ok,
                        format!("{detail} {note}").trim(),
                    );
                    result
                })
                .await;
            let _ = this.update(cx, |store, cx| {
                match outcome {
                    Ok(_) => {
                        store.epoch += 1;
                        store.last_error = None;
                        store.refresh(root, cx);
                    }
                    Err(err) => {
                        tracing::warn!(target: "piku::git", %err, "git operation failed");
                        // Defense in depth: even though every GitError is
                        // built from sanitized text today, re-sanitize at
                        // the display boundary so a future unsanitized
                        // error path cannot reach the UI.
                        store.last_error = Some(crate::security::git_text::sanitize_git_text(
                            &err.to_string(),
                            300,
                            false,
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn stage(&mut self, root: PathBuf, rels: Vec<PathBuf>, cx: &mut Context<Self>) {
        let detail = format!("{} path(s)", rels.len());
        self.run_mutation(root.clone(), "git.stage", detail, cx, move |backend| {
            use super::backend::GitBackend as _;
            backend.stage(&root, &rels).map(|_| String::new())
        });
    }

    pub fn unstage(&mut self, root: PathBuf, rels: Vec<PathBuf>, cx: &mut Context<Self>) {
        let detail = format!("{} path(s)", rels.len());
        self.run_mutation(root.clone(), "git.unstage", detail, cx, move |backend| {
            use super::backend::GitBackend as _;
            backend.unstage(&root, &rels).map(|_| String::new())
        });
    }

    pub fn commit(&mut self, root: PathBuf, message: String, cx: &mut Context<Self>) {
        self.run_mutation(
            root.clone(),
            "git.commit",
            String::new(),
            cx,
            move |backend| {
                use super::backend::GitBackend as _;
                backend.commit_create(&root, &message)
            },
        );
    }

    pub fn checkout(&mut self, root: PathBuf, branch: String, cx: &mut Context<Self>) {
        let detail = branch.clone();
        self.run_mutation(root.clone(), "git.checkout", detail, cx, move |backend| {
            use super::backend::GitBackend as _;
            let interrupt = Arc::new(AtomicBool::new(false));
            backend.checkout(&root, &branch, &interrupt).map(|_| branch)
        });
    }

    pub fn create_branch(&mut self, root: PathBuf, name: String, cx: &mut Context<Self>) {
        let detail = name.clone();
        self.run_mutation(
            root.clone(),
            "git.branch.create",
            detail,
            cx,
            move |backend| {
                use super::backend::GitBackend as _;
                backend.create_branch(&root, &name).map(|_| name)
            },
        );
    }

    pub fn delete_branch(&mut self, root: PathBuf, name: String, cx: &mut Context<Self>) {
        let detail = name.clone();
        self.run_mutation(
            root.clone(),
            "git.branch.delete",
            detail,
            cx,
            move |backend| {
                use super::backend::GitBackend as _;
                backend.delete_branch(&root, &name).map(|_| name)
            },
        );
    }
}
