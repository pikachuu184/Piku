//! The gitoxide implementation of [`GitBackend`]. This is the only module
//! that touches gix types — everything it returns is plain sanitized data.
//!
//! Security posture:
//! - Repositories are opened with `gix::open::Options::isolated()`: no
//!   environment overrides, no global/system config, sensitive keys (paths to
//!   executables) skipped. No hook, filter driver, or external diff/merge
//!   tool is ever invoked.
//! - Discovery walks upward manually and is ceilinged by the `PathGuard`
//!   roots; `.git` *files* (worktree/submodule redirections) are not followed.
//! - Every repo-relative path is re-validated through the guard before it is
//!   allowed to describe a filesystem location.
//! - Every repo-derived string passes `sanitize_git_text` before leaving.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::channel::mpsc::UnboundedSender;
use gix::bstr::{BStr, BString, ByteSlice};

use crate::security::git_text::sanitize_git_text;
use crate::security::path_guard::PathGuard;
use crate::services::jobs::JobEvent;

use super::backend::{GitBackend, GitError, GitResult};
use super::types::{
    BranchInfo, ChangedFile, CommitDetail, CommitInfo, DiffHunk, DiffLineKind, DiffPayload,
    DiffTarget, FetchOutcome, GitFileStatus, GitStatusCode, RemoteInfo, RepoSnapshot,
    StatusSnapshot,
};
use super::{
    AHEAD_BEHIND_CAP, BLOB_PREVIEW_CAP, COMMIT_PAGE, DIFF_MAX_BYTES, FILE_HISTORY_WALK_CAP,
    MAX_AUTHOR_CHARS, MAX_BODY_CHARS, MAX_REF_CHARS, MAX_SUMMARY_CHARS, MAX_URL_CHARS,
    STATUS_MAX_ENTRIES,
};

/// Diff rendering caps (module-local; the byte caps live in `super`).
const DIFF_CONTEXT_LINES: usize = 3;
/// Render-bound: the diff panel is not virtualized, so every line is an
/// element — keep the ceiling low enough that huge diffs stay smooth.
const DIFF_MAX_LINES: usize = 1_500;
const DIFF_MAX_LINE_CHARS: usize = 400;
/// Changed files listed per commit before truncation.
const COMMIT_CHANGES_CAP: usize = 1_000;

#[cfg(test)]
#[path = "gix_backend_tests.rs"]
mod tests;

pub struct GixBackend {
    pub(super) guard: PathGuard,
    /// Cached thread-safe handles keyed by sanitized worktree root. The
    /// cached handle is `Send + Sync`; each operation derives a cheap
    /// thread-local `Repository` from it on the calling worker thread.
    repos: Mutex<HashMap<PathBuf, Arc<gix::ThreadSafeRepository>>>,
}

impl Default for GixBackend {
    fn default() -> Self {
        Self {
            guard: PathGuard::with_system_roots(),
            repos: Mutex::new(HashMap::new()),
        }
    }
}

impl GixBackend {
    /// Open (or reuse) the repository at `root` in isolated mode and return a
    /// thread-local handle for the current worker thread.
    fn repo(&self, root: &Path) -> GitResult<gix::Repository> {
        let root = self.guard.sanitize(root).map_err(GitError::msg)?;
        if let Some(cached) = self.repos.lock().ok().and_then(|m| m.get(&root).cloned()) {
            return Ok(cached.to_thread_local());
        }
        let shared = gix::open::Options::isolated()
            .open(&root)
            .map_err(GitError::msg)?;
        let shared = Arc::new(shared);
        if let Ok(mut map) = self.repos.lock() {
            map.insert(root, shared.clone());
        }
        Ok(shared.to_thread_local())
    }

    /// Drop a cached handle (e.g. when `.git` disappeared).
    pub fn evict(&self, root: &Path) {
        if let (Ok(mut map), Ok(root)) = (self.repos.lock(), self.guard.sanitize(root)) {
            map.remove(&root);
        }
    }

    /// Turn a repo-relative slash path into a guarded absolute path, or
    /// `None` when the result would escape the authorized roots.
    fn abs_path(&self, root: &Path, rela: &BStr) -> Option<PathBuf> {
        let rela = rela.to_str().ok()?;
        if rela.is_empty() || rela.contains('\0') {
            return None;
        }
        let joined = root.join(rela.replace('/', std::path::MAIN_SEPARATOR_STR));
        self.guard.sanitize(&joined).ok()
    }
}

/// Sanitize a ref-like short name for display.
fn clean_ref(raw: &BStr) -> String {
    sanitize_git_text(&raw.to_str_lossy(), MAX_REF_CHARS, false)
}

fn commit_time_to_system(seconds: i64) -> SystemTime {
    if seconds >= 0 {
        UNIX_EPOCH + Duration::from_secs(seconds as u64)
    } else {
        UNIX_EPOCH
    }
}

/// Parse a user/history-provided commit id: strictly hex, bounded length.
/// This deliberately refuses arbitrary revspecs — history UI only ever hands
/// back ids the backend itself produced.
fn parse_commit_id(repo: &gix::Repository, id: &str) -> GitResult<gix::ObjectId> {
    if id.len() < 4 || id.len() > 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitError::Message("invalid commit id".into()));
    }
    let spec = repo
        .rev_parse_single(BString::from(id).as_bstr())
        .map_err(GitError::msg)?;
    Ok(spec.detach())
}

/// Resolve a branch argument to a full `refs/heads/…` name. Accepts either a
/// full ref name coming from `BranchInfo::ref_name` (validated structurally:
/// prefix, no control bytes, no traversal) or a user-typed short name gated
/// by the stricter `validate_branch_name`. Never rendered — operations only.
fn resolve_branch_ref(branch: &str) -> GitResult<String> {
    if let Some(short) = branch.strip_prefix("refs/heads/") {
        if short.is_empty()
            || branch.len() > 400
            || branch.chars().any(|c| c.is_control())
            || branch.contains("..")
            || branch.contains('\\')
        {
            return Err(GitError::Message("invalid branch reference".into()));
        }
        Ok(branch.to_string())
    } else {
        crate::security::git_text::validate_branch_name(branch).map_err(GitError::Message)?;
        Ok(format!("refs/heads/{branch}"))
    }
}

/// Interrupt check helper for loops gix cannot interrupt itself.
fn check_interrupt(flag: &Arc<AtomicBool>) -> GitResult<()> {
    if flag.load(Ordering::Relaxed) {
        Err(GitError::Interrupted)
    } else {
        Ok(())
    }
}

impl GixBackend {
    fn commit_info(
        &self,
        repo: &gix::Repository,
        id: gix::ObjectId,
        refs: &HashMap<gix::ObjectId, Vec<String>>,
    ) -> GitResult<CommitInfo> {
        let commit = repo.find_commit(id).map_err(GitError::msg)?;
        let message = commit.message().map_err(GitError::msg)?;
        let summary =
            sanitize_git_text(&message.summary().to_str_lossy(), MAX_SUMMARY_CHARS, false);
        let author = commit
            .author()
            .map(|a| sanitize_git_text(&a.name.to_str_lossy(), MAX_AUTHOR_CHARS, false))
            .unwrap_or_default();
        let time = commit
            .committer()
            .ok()
            .and_then(|c| c.time().ok())
            .map(|t| commit_time_to_system(t.seconds))
            .unwrap_or(UNIX_EPOCH);
        let short = commit
            .short_id()
            .map(|p| p.to_string())
            .unwrap_or_else(|_| id.to_hex_with_len(8).to_string());
        Ok(CommitInfo {
            id: id.to_string(),
            short,
            summary,
            author,
            time,
            refs: refs.get(&id).cloned().unwrap_or_default(),
        })
    }

    /// Map of commit id → decorations (branch/tag short names).
    fn ref_decorations(&self, repo: &gix::Repository) -> HashMap<gix::ObjectId, Vec<String>> {
        let mut map: HashMap<gix::ObjectId, Vec<String>> = HashMap::new();
        let Ok(platform) = repo.references() else {
            return map;
        };
        for prefix in ["refs/heads/", "refs/tags/"] {
            let Ok(iter) = platform.prefixed(prefix) else {
                continue;
            };
            for reference in iter.flatten() {
                let mut reference = reference;
                let Ok(id) = reference.peel_to_id() else {
                    continue;
                };
                let name = clean_ref(reference.name().shorten());
                let entry = map.entry(id.detach()).or_default();
                if entry.len() < 8 {
                    entry.push(name);
                }
            }
        }
        map
    }

    /// Bytes of `rela` (slash-separated) in `tree`, capped. `Ok(None)` when
    /// the path does not exist in the tree.
    fn tree_blob_bytes(
        &self,
        repo: &gix::Repository,
        tree: &gix::Tree<'_>,
        rela: &BStr,
        cap: usize,
    ) -> GitResult<Option<(Vec<u8>, bool)>> {
        let entry = tree
            .lookup_entry_by_path(rela.to_str_lossy().as_ref())
            .map_err(GitError::msg)?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        if !entry.mode().is_blob() {
            return Ok(None);
        }
        let blob = repo.find_blob(entry.object_id()).map_err(GitError::msg)?;
        let truncated = blob.data.len() > cap;
        let mut bytes = blob.data.clone();
        bytes.truncate(cap);
        Ok(Some((bytes, truncated)))
    }
}

impl GitBackend for GixBackend {
    fn discover(&self, dir: &Path) -> Option<PathBuf> {
        // Sanitizing first guarantees an absolute, traversal-free start point
        // inside an authorized root; the upward walk can then never escape.
        let start = self.guard.sanitize(dir).ok()?;
        let mut cursor: &Path = &start;
        loop {
            let dot_git = cursor.join(".git");
            // Only `.git` directories are accepted: gitfile redirections
            // (linked worktrees, submodules) may point anywhere on disk and
            // are deferred until they can be validated properly.
            if dot_git.is_dir() {
                let root = self.guard.sanitize(cursor).ok()?;
                // Confirm it actually opens as a repository before reporting.
                return self.repo(&root).ok().map(|_| root);
            }
            match cursor.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => cursor = parent,
                _ => return None,
            }
        }
    }

    fn snapshot(&self, root: &Path, interrupt: &Arc<AtomicBool>) -> GitResult<RepoSnapshot> {
        let repo = self.repo(root)?;
        let mut snap = RepoSnapshot {
            root: self.guard.sanitize(root).map_err(GitError::msg)?,
            ..Default::default()
        };

        let head = repo.head().map_err(GitError::msg)?;
        if let Some(name) = head.referent_name() {
            snap.branch = Some(clean_ref(name.shorten()));
        } else if let Some(id) = head.id() {
            snap.detached_short = Some(id.to_hex_with_len(8).to_string());
        }

        // Multi-step operation in progress (merge/rebase/…): static labels.
        snap.in_progress = repo.state().map(|state| {
            use gix::state::InProgress;
            match state {
                InProgress::ApplyMailbox | InProgress::ApplyMailboxRebase => "applying patches",
                InProgress::Bisect => "bisecting",
                InProgress::CherryPick | InProgress::CherryPickSequence => "cherry-picking",
                InProgress::Merge => "merging",
                InProgress::Rebase | InProgress::RebaseInteractive => "rebasing",
                InProgress::Revert | InProgress::RevertSequence => "reverting",
            }
        });

        // Remotes, with the https allowlist evaluated up front so the UI can
        // grey out anything it must never fetch from.
        for name in repo.remote_names().iter() {
            let Ok(remote) = repo.find_remote(name.as_ref()) else {
                continue;
            };
            let Some(url) = remote.url(gix::remote::Direction::Fetch) else {
                continue;
            };
            let fetchable = matches!(url.scheme, gix::url::Scheme::Https);
            snap.remotes.push(RemoteInfo {
                name: clean_ref(name.as_ref()),
                url: sanitize_git_text(&url.to_bstring().to_str_lossy(), MAX_URL_CHARS, false),
                fetchable,
            });
        }

        // Ahead/behind against the tracked upstream, both walks capped.
        'sync: {
            let Ok(Some(head_ref)) = repo.head_name() else {
                break 'sync;
            };
            let Some(Ok(tracking)) = repo
                .branch_remote_tracking_ref_name(head_ref.as_ref(), gix::remote::Direction::Fetch)
            else {
                break 'sync;
            };
            let Ok(upstream) = repo.find_reference(tracking.as_ref().as_bstr()) else {
                break 'sync;
            };
            let mut upstream = upstream;
            let (Ok(local_id), Ok(upstream_id)) = (repo.head_id(), upstream.peel_to_id()) else {
                break 'sync;
            };
            let count = |tip: gix::ObjectId, hide: gix::ObjectId| -> GitResult<usize> {
                let walk = repo
                    .rev_walk([tip])
                    .with_hidden([hide])
                    .all()
                    .map_err(GitError::msg)?;
                let mut n = 0usize;
                for info in walk {
                    check_interrupt(interrupt)?;
                    if info.is_err() {
                        break;
                    }
                    n += 1;
                    if n >= AHEAD_BEHIND_CAP {
                        break;
                    }
                }
                Ok(n)
            };
            snap.ahead = Some(count(local_id.detach(), upstream_id.detach())?);
            snap.behind = Some(count(upstream_id.detach(), local_id.detach())?);
        }

        // Cleanliness counts are merged in by the store from the status
        // snapshot so the (expensive) walk runs exactly once per refresh.
        Ok(snap)
    }

    fn status(&self, root: &Path, interrupt: &Arc<AtomicBool>) -> GitResult<StatusSnapshot> {
        let repo = self.repo(root)?;
        let root = self.guard.sanitize(root).map_err(GitError::msg)?;

        let iter = repo
            .status(gix::progress::Discard)
            .map_err(GitError::msg)?
            .should_interrupt_owned(interrupt.clone())
            .untracked_files(gix::status::UntrackedFiles::Files)
            // Ignored entries are informational decorations; collapsing to
            // directories keeps e.g. `target/` at one entry, not thousands.
            .dirwalk_options(|options| {
                options.emit_ignored(Some(gix::dir::walk::EmissionMode::CollapseDirectory))
            })
            .into_iter(None::<BString>)
            .map_err(GitError::msg)?;

        let mut by_path: HashMap<PathBuf, GitFileStatus> = HashMap::new();
        let mut ignored: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut count = 0usize;

        for item in iter {
            let Ok(item) = item else { continue };
            if count >= STATUS_MAX_ENTRIES {
                break;
            }
            // Ignored entries live in their own (separately capped) set and
            // never touch the dirty map or its budget.
            if let gix::status::Item::IndexWorktree(
                gix::status::index_worktree::Item::DirectoryContents { entry, .. },
            ) = &item
                && matches!(entry.status, gix::dir::entry::Status::Ignored(_))
            {
                if ignored.len() < super::IGNORED_MAX_ENTRIES
                    && let Some(abs) = self.abs_path(&root, entry.rela_path.as_bstr())
                {
                    ignored.insert(abs);
                }
                continue;
            }
            let location = item.location().to_owned();
            let Some(abs) = self.abs_path(&root, location.as_bstr()) else {
                continue;
            };
            let entry = by_path.entry(abs).or_default();
            match &item {
                gix::status::Item::IndexWorktree(iw) => {
                    use gix::status::index_worktree::iter::Summary;
                    let Some(summary) = iw.summary() else {
                        continue;
                    };
                    let code = match summary {
                        Summary::Removed => GitStatusCode::Deleted,
                        Summary::Added => GitStatusCode::Untracked,
                        Summary::Modified | Summary::TypeChange => GitStatusCode::Modified,
                        Summary::Renamed | Summary::Copied => GitStatusCode::Renamed,
                        Summary::IntentToAdd => GitStatusCode::Added,
                        Summary::Conflict => GitStatusCode::Conflicted,
                    };
                    entry.worktree = Some(code);
                }
                gix::status::Item::TreeIndex(change) => {
                    use gix::diff::index::ChangeRef;
                    let code = match change {
                        ChangeRef::Addition { .. } => GitStatusCode::Added,
                        ChangeRef::Deletion { .. } => GitStatusCode::Deleted,
                        ChangeRef::Modification { .. } => GitStatusCode::Modified,
                        ChangeRef::Rewrite { .. } => GitStatusCode::Renamed,
                    };
                    entry.index = Some(code);
                }
            }
            count += 1;
        }
        check_interrupt(interrupt)?;

        // Dirty-directory rollup: every ancestor of a dirty file (up to and
        // including the repo root) gets a dot badge in the file list.
        let mut dirty_dirs = std::collections::HashSet::new();
        for path in by_path.keys() {
            let mut cursor = path.parent();
            while let Some(dir) = cursor {
                if !dir.starts_with(&root) || !dirty_dirs.insert(dir.to_path_buf()) {
                    break;
                }
                if dir == root {
                    break;
                }
                cursor = dir.parent();
            }
        }

        Ok(StatusSnapshot {
            by_path: Arc::new(by_path),
            dirty_dirs: Arc::new(dirty_dirs),
            ignored: Arc::new(ignored),
        })
    }

    fn commits(
        &self,
        root: &Path,
        before: Option<&str>,
        page: usize,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<Vec<CommitInfo>> {
        let repo = self.repo(root)?;
        let page = page.clamp(1, COMMIT_PAGE);
        let decorations = self.ref_decorations(&repo);

        let tip = match before {
            Some(id) => parse_commit_id(&repo, id)?,
            None => match repo.head_id() {
                Ok(id) => id.detach(),
                // Unborn branch (no commits yet).
                Err(_) => return Ok(Vec::new()),
            },
        };

        let walk = repo.rev_walk([tip]).all().map_err(GitError::msg)?;
        let mut out = Vec::with_capacity(page);
        for (ix, info) in walk.enumerate() {
            check_interrupt(interrupt)?;
            let Ok(info) = info else { break };
            // When paging, the `before` commit itself was already delivered.
            if before.is_some() && ix == 0 {
                continue;
            }
            out.push(self.commit_info(&repo, info.id, &decorations)?);
            if out.len() >= page {
                break;
            }
        }
        Ok(out)
    }

    fn commit_detail(
        &self,
        root: &Path,
        id: &str,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<CommitDetail> {
        let repo = self.repo(root)?;
        let sanitized_root = self.guard.sanitize(root).map_err(GitError::msg)?;
        let oid = parse_commit_id(&repo, id)?;
        let decorations = self.ref_decorations(&repo);
        let info = self.commit_info(&repo, oid, &decorations)?;

        let commit = repo.find_commit(oid).map_err(GitError::msg)?;
        let body = commit
            .message()
            .ok()
            .and_then(|m| {
                m.body
                    .map(|b| sanitize_git_text(&b.to_str_lossy(), MAX_BODY_CHARS, true))
            })
            .unwrap_or_default();

        let tree = commit.tree().map_err(GitError::msg)?;
        let parent_tree = commit
            .parent_ids()
            .next()
            .and_then(|pid| repo.find_commit(pid.detach()).ok())
            .and_then(|parent| parent.tree().ok());

        check_interrupt(interrupt)?;
        let changes = repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
            .map_err(GitError::msg)?;

        let truncated = changes.len() > COMMIT_CHANGES_CAP;
        let mut out = Vec::with_capacity(changes.len().min(COMMIT_CHANGES_CAP));
        for change in changes.into_iter().take(COMMIT_CHANGES_CAP) {
            use gix::diff::tree_with_rewrites::Change;
            let (location, code) = match &change {
                Change::Addition { location, .. } => (location, GitStatusCode::Added),
                Change::Deletion { location, .. } => (location, GitStatusCode::Deleted),
                Change::Modification { location, .. } => (location, GitStatusCode::Modified),
                Change::Rewrite { location, .. } => (location, GitStatusCode::Renamed),
            };
            out.push(ChangedFile {
                rel_path: sanitize_git_text(&location.to_str_lossy(), MAX_REF_CHARS, false),
                abs_path: self.abs_path(&sanitized_root, location.as_bstr()),
                code,
            });
        }

        Ok(CommitDetail {
            info,
            body,
            changes: out,
            truncated,
        })
    }

    fn file_history(
        &self,
        root: &Path,
        rel_path: &Path,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<Vec<CommitInfo>> {
        let repo = self.repo(root)?;
        let rela: String = rel_path
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if rela.is_empty() || rela.contains('\0') {
            return Err(GitError::Message("invalid path".into()));
        }
        let decorations = self.ref_decorations(&repo);

        let Ok(head_id) = repo.head_id() else {
            return Ok(Vec::new());
        };

        // First-parent walk: linear history of the current branch. Collect
        // (commit, blob-id-at-commit) pairs, then emit commits whose blob id
        // differs from their (first) parent's.
        let walk = repo
            .rev_walk([head_id.detach()])
            .first_parent_only()
            .all()
            .map_err(GitError::msg)?;

        let mut chain: Vec<(gix::ObjectId, Option<gix::ObjectId>)> = Vec::new();
        for info in walk.take(FILE_HISTORY_WALK_CAP) {
            check_interrupt(interrupt)?;
            let Ok(info) = info else { break };
            let entry_id = repo
                .find_commit(info.id)
                .ok()
                .and_then(|c| c.tree().ok())
                .and_then(|t| t.lookup_entry_by_path(rela.as_str()).ok().flatten())
                .map(|e| e.object_id());
            chain.push((info.id, entry_id));
        }

        let mut out = Vec::new();
        for ix in 0..chain.len() {
            let (commit_id, entry) = &chain[ix];
            let parent_entry = chain.get(ix + 1).map(|(_, e)| *e);
            let changed = match parent_entry {
                Some(parent) => *entry != parent,
                // Oldest scanned commit: only meaningful when the file exists
                // there and the walk reached the true root of history.
                None => entry.is_some() && chain.len() < FILE_HISTORY_WALK_CAP,
            };
            if changed && (entry.is_some() || parent_entry.flatten().is_some()) {
                out.push(self.commit_info(&repo, *commit_id, &decorations)?);
            }
        }
        Ok(out)
    }

    fn diff_file(
        &self,
        root: &Path,
        target: &DiffTarget,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<DiffPayload> {
        let repo = self.repo(root)?;
        let sanitized_root = self.guard.sanitize(root).map_err(GitError::msg)?;
        let cap = DIFF_MAX_BYTES as usize;

        // Resolve both sides as (label, bytes, truncated).
        let (old_label, new_label, old_bytes, new_bytes, mut truncated) = match target {
            DiffTarget::WorktreeVsIndex { rel_path } => {
                let rela = rel_to_slash(rel_path)?;
                let index = repo.index_or_empty().map_err(GitError::msg)?;
                let old = index
                    .entry_by_path(rela.as_bstr())
                    .map(|entry| self.blob_bytes(&repo, entry.id, cap))
                    .transpose()?
                    .unwrap_or((Vec::new(), false));
                let new = self.worktree_bytes(&sanitized_root, rel_path, cap)?;
                (
                    "index".to_string(),
                    "working tree".to_string(),
                    old.0,
                    new.0,
                    old.1 || new.1,
                )
            }
            DiffTarget::IndexVsHead { rel_path } => {
                let rela = rel_to_slash(rel_path)?;
                let old = match repo.head_commit().ok().and_then(|c| c.tree().ok()) {
                    Some(tree) => self
                        .tree_blob_bytes(&repo, &tree, rela.as_bstr(), cap)?
                        .unwrap_or((Vec::new(), false)),
                    None => (Vec::new(), false),
                };
                let index = repo.index_or_empty().map_err(GitError::msg)?;
                let new = index
                    .entry_by_path(rela.as_bstr())
                    .map(|entry| self.blob_bytes(&repo, entry.id, cap))
                    .transpose()?
                    .unwrap_or((Vec::new(), false));
                (
                    "HEAD".to_string(),
                    "index".to_string(),
                    old.0,
                    new.0,
                    old.1 || new.1,
                )
            }
            DiffTarget::CommitVsParent { commit, rel_path } => {
                let rela = rel_to_slash(rel_path)?;
                let oid = parse_commit_id(&repo, commit)?;
                let commit_obj = repo.find_commit(oid).map_err(GitError::msg)?;
                let tree = commit_obj.tree().map_err(GitError::msg)?;
                let new = self
                    .tree_blob_bytes(&repo, &tree, rela.as_bstr(), cap)?
                    .unwrap_or((Vec::new(), false));
                let old = match commit_obj
                    .parent_ids()
                    .next()
                    .and_then(|pid| repo.find_commit(pid.detach()).ok())
                    .and_then(|parent| parent.tree().ok())
                {
                    Some(parent_tree) => self
                        .tree_blob_bytes(&repo, &parent_tree, rela.as_bstr(), cap)?
                        .unwrap_or((Vec::new(), false)),
                    None => (Vec::new(), false),
                };
                let short = oid.to_hex_with_len(8).to_string();
                (format!("{short}^"), short, old.0, new.0, old.1 || new.1)
            }
        };
        check_interrupt(interrupt)?;

        // Binary sniff: a NUL in the head of either side means no text diff.
        let sniff = |bytes: &[u8]| bytes.iter().take(8192).any(|&b| b == 0);
        if sniff(&old_bytes) || sniff(&new_bytes) {
            return Ok(DiffPayload {
                old_label,
                new_label,
                hunks: Vec::new(),
                truncated,
                note: Some("binary content — no text diff".into()),
            });
        }

        let (hunks, line_capped) = compute_hunks(&old_bytes, &new_bytes);
        truncated |= line_capped;
        let note = if hunks.is_empty() {
            Some(if truncated {
                "content too large to diff".to_string()
            } else {
                "no changes".to_string()
            })
        } else {
            None
        };
        Ok(DiffPayload {
            old_label,
            new_label,
            hunks,
            truncated,
            note,
        })
    }

    fn blob_at(
        &self,
        root: &Path,
        commit: &str,
        rel_path: &Path,
        cap: usize,
    ) -> GitResult<(Vec<u8>, bool)> {
        let repo = self.repo(root)?;
        let rela = rel_to_slash(rel_path)?;
        let oid = parse_commit_id(&repo, commit)?;
        let commit_obj = repo.find_commit(oid).map_err(GitError::msg)?;
        let tree = commit_obj.tree().map_err(GitError::msg)?;
        let cap = cap.min(BLOB_PREVIEW_CAP);
        self.tree_blob_bytes(&repo, &tree, rela.as_bstr(), cap)?
            .ok_or_else(|| GitError::Message("file does not exist in that revision".into()))
    }

    fn branches(&self, root: &Path) -> GitResult<Vec<BranchInfo>> {
        let repo = self.repo(root)?;
        let head_name = repo.head_name().ok().flatten();
        let platform = repo.references().map_err(GitError::msg)?;
        let mut out = Vec::new();

        if let Ok(iter) = platform.local_branches() {
            for reference in iter.flatten() {
                let full = reference.name().to_owned();
                let upstream = repo
                    .branch_remote_tracking_ref_name(full.as_ref(), gix::remote::Direction::Fetch)
                    .and_then(|r| r.ok())
                    .map(|tracking| clean_ref(tracking.as_ref().shorten()));
                out.push(BranchInfo {
                    name: clean_ref(full.as_ref().shorten()),
                    // Ops-only lossy form; exotic (non-UTF8) ref bytes are
                    // beyond scope and simply won't round-trip.
                    ref_name: full.as_bstr().to_str_lossy().into_owned(),
                    // Byte-form comparison, same as `delete_branch` — keeps the
                    // HEAD check independent of `FullName`'s structural equality.
                    is_head: head_name.as_ref().map(|h| h.as_bstr()) == Some(full.as_bstr()),
                    is_remote: false,
                    upstream,
                });
            }
        }
        if let Ok(iter) = platform.remote_branches() {
            for reference in iter.flatten() {
                // Skip symbolic refs like `origin/HEAD`.
                if matches!(reference.target(), gix::refs::TargetRef::Symbolic(_)) {
                    continue;
                }
                out.push(BranchInfo {
                    name: clean_ref(reference.name().shorten()),
                    ref_name: reference.name().as_bstr().to_str_lossy().into_owned(),
                    is_head: false,
                    is_remote: true,
                    upstream: None,
                });
            }
        }
        Ok(out)
    }

    /// Switch branches. v1 policy: refuses when the worktree or index is
    /// dirty (no auto-stash), refuses to overwrite untracked collisions.
    fn checkout(&self, root: &Path, branch: &str, interrupt: &Arc<AtomicBool>) -> GitResult<()> {
        let full_name = resolve_branch_ref(branch)?;
        let repo = self.repo(root)?;
        let sanitized_root = self.guard.sanitize(root).map_err(GitError::msg)?;

        if repo.is_dirty().map_err(GitError::msg)? {
            return Err(GitError::DirtyWorktree);
        }

        // Resolve the target branch to a commit + tree.
        let mut target_ref = repo
            .find_reference(full_name.as_str())
            .map_err(|_| GitError::Message("branch does not exist".into()))?;
        let target_id = target_ref.peel_to_id().map_err(GitError::msg)?.detach();
        let target_tree = repo
            .find_commit(target_id)
            .map_err(GitError::msg)?
            .tree_id()
            .map_err(GitError::msg)?
            .detach();

        // Files present in the current HEAD tree but absent from the target
        // tree must be removed from the worktree (the tree diff also covers
        // renames via their Deletion/Addition halves since rewrites are off
        // here — pass explicit options with rewrites disabled).
        let head_tree = repo.head_commit().ok().and_then(|c| c.tree().ok());
        if let Some(head_tree) = &head_tree {
            let target_tree_obj = repo.find_tree(target_tree).map_err(GitError::msg)?;
            let changes = repo
                .diff_tree_to_tree(
                    Some(head_tree),
                    Some(&target_tree_obj),
                    gix::diff::Options::default(),
                )
                .map_err(GitError::msg)?;
            for change in changes {
                check_interrupt(interrupt)?;
                use gix::diff::tree_with_rewrites::Change;
                let gone = match &change {
                    Change::Deletion { location, .. } => Some(location),
                    Change::Rewrite {
                        source_location, ..
                    } => Some(source_location),
                    _ => None,
                };
                if let Some(location) = gone
                    && let Some(abs) = self.abs_path(&sanitized_root, location.as_bstr())
                    && std::fs::symlink_metadata(&abs)
                        .map(|m| m.is_file())
                        .unwrap_or(false)
                {
                    std::fs::remove_file(&abs).map_err(GitError::msg)?;
                }
            }
        }

        // Materialize the target tree over the (clean) worktree.
        let mut index = repo.index_from_tree(&target_tree).map_err(GitError::msg)?;
        let options = repo
            .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
            .map_err(GitError::msg)?;
        let workdir = repo
            .workdir()
            .ok_or_else(|| GitError::Message("repository has no working directory".into()))?
            .to_path_buf();
        gix::worktree::state::checkout(
            &mut index,
            workdir,
            repo.objects.clone(),
            &gix::progress::Discard,
            &gix::progress::Discard,
            interrupt,
            options,
        )
        .map_err(GitError::msg)?;
        check_interrupt(interrupt)?;
        index.write(Default::default()).map_err(GitError::msg)?;

        // Point HEAD at the branch (with a reflog entry). The reflog line
        // needs an identity even in isolated mode, so one is always supplied.
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        let symbolic: gix::refs::FullName = full_name.as_str().try_into().map_err(GitError::msg)?;
        let sig = self.reflog_identity(&repo, root);
        let mut time_buf = Default::default();
        repo.edit_references_as(
            Some(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: format!("checkout: moving to {branch}").into(),
                    },
                    expected: PreviousValue::Any,
                    new: gix::refs::Target::Symbolic(symbolic),
                },
                name: "HEAD".try_into().map_err(GitError::msg)?,
                deref: false,
            }),
            Some(sig.to_ref(&mut time_buf)),
        )
        .map_err(GitError::msg)?;
        Ok(())
    }

    fn create_branch(&self, root: &Path, name: &str) -> GitResult<()> {
        crate::security::git_text::validate_branch_name(name).map_err(GitError::Message)?;
        let repo = self.repo(root)?;
        let head_id = repo
            .head_id()
            .map_err(|_| GitError::Message("repository has no commits yet".into()))?;
        // Explicit existence check: the transaction alone treats an update to
        // an identical target as a no-op instead of a conflict.
        if let Ok(Some(_)) = repo.try_find_reference(format!("refs/heads/{name}").as_str()) {
            return Err(GitError::Message(format!("branch `{name}` already exists")));
        }
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
        let sig = self.reflog_identity(&repo, root);
        let mut time_buf = Default::default();
        repo.edit_references_as(
            Some(RefEdit {
                change: Change::Update {
                    log: LogChange {
                        mode: RefLog::AndReference,
                        force_create_reflog: false,
                        message: format!("branch: created {name}").into(),
                    },
                    expected: PreviousValue::MustNotExist,
                    new: gix::refs::Target::Object(head_id.detach()),
                },
                name: format!("refs/heads/{name}")
                    .as_str()
                    .try_into()
                    .map_err(GitError::msg)?,
                deref: false,
            }),
            Some(sig.to_ref(&mut time_buf)),
        )
        .map_err(GitError::msg)?;
        Ok(())
    }

    fn delete_branch(&self, root: &Path, name: &str) -> GitResult<()> {
        let full = resolve_branch_ref(name)?;
        let repo = self.repo(root)?;
        if let Ok(Some(head_name)) = repo.head_name()
            && head_name.as_bstr() == full.as_str()
        {
            return Err(GitError::Message(
                "cannot delete the checked-out branch".into(),
            ));
        }
        let reference = repo
            .find_reference(full.as_str())
            .map_err(|_| GitError::Message("branch does not exist".into()))?;
        reference.delete().map_err(GitError::msg)?;
        Ok(())
    }

    /// Stage paths: write worktree contents as blobs and upsert their index
    /// entries. A missing worktree file stages the deletion. No clean/smudge
    /// filters and no eol conversion run — content is staged byte-for-byte
    /// (the isolated-config equivalent of `core.autocrlf=false`), which is a
    /// deliberate security posture, not an omission.
    fn stage(&self, root: &Path, rel_paths: &[PathBuf]) -> GitResult<()> {
        let repo = self.repo(root)?;
        let sanitized_root = self.guard.sanitize(root).map_err(GitError::msg)?;
        let mut index = self.open_or_new_index(&repo)?;

        for rel in rel_paths {
            let rela = rel_to_slash(rel)?;
            let abs = self
                .guard
                .sanitize(&sanitized_root.join(rel))
                .map_err(GitError::msg)?;
            match std::fs::symlink_metadata(&abs) {
                Ok(meta) if meta.is_file() => {
                    let file = std::fs::File::open(&abs).map_err(GitError::msg)?;
                    let id = repo
                        .write_blob_stream(std::io::BufReader::new(file))
                        .map_err(GitError::msg)?
                        .detach();
                    let stat = gix::index::entry::Stat::from_fs(
                        &gix::index::fs::Metadata::from_path_no_follow(&abs)
                            .map_err(GitError::msg)?,
                    )
                    .map_err(GitError::msg)?;
                    if let Ok(existing) = index.entry_index_by_path(rela.as_bstr()) {
                        let entry = &mut index.entries_mut()[existing];
                        entry.stat = stat;
                        entry.id = id;
                        entry.mode = gix::index::entry::Mode::FILE;
                    } else {
                        index.dangerously_push_entry(
                            stat,
                            id,
                            gix::index::entry::Flags::empty(),
                            gix::index::entry::Mode::FILE,
                            rela.as_bstr(),
                        );
                        index.sort_entries();
                    }
                }
                Ok(_) => {
                    // Symlinks/dirs are never staged by PIKU (safety).
                    return Err(GitError::Message(format!(
                        "`{}` is not a regular file",
                        rel.display()
                    )));
                }
                Err(_) => {
                    // Vanished from the worktree → stage the deletion.
                    if let Ok(existing) = index.entry_index_by_path(rela.as_bstr()) {
                        index.remove_entry_at_index(existing);
                    }
                }
            }
        }
        index.write(Default::default()).map_err(GitError::msg)?;
        Ok(())
    }

    /// Unstage paths: restore their index entries from HEAD (or drop them
    /// when HEAD does not know the path).
    fn unstage(&self, root: &Path, rel_paths: &[PathBuf]) -> GitResult<()> {
        let repo = self.repo(root)?;
        let mut index = self.open_or_new_index(&repo)?;
        let head_tree = repo.head_commit().ok().and_then(|c| c.tree().ok());

        for rel in rel_paths {
            let rela = rel_to_slash(rel)?;
            let head_entry = match &head_tree {
                Some(tree) => tree
                    .lookup_entry_by_path(rela.to_str_lossy().as_ref())
                    .map_err(GitError::msg)?,
                None => None,
            };
            match head_entry {
                Some(entry) if entry.mode().is_blob() => {
                    // Zeroed stat forces a content re-check on next status —
                    // correct, just marginally slower once.
                    let (id, mode) = (entry.object_id(), gix::index::entry::Mode::FILE);
                    if let Ok(existing) = index.entry_index_by_path(rela.as_bstr()) {
                        let entry = &mut index.entries_mut()[existing];
                        entry.id = id;
                        entry.mode = mode;
                        entry.stat = Default::default();
                    } else {
                        index.dangerously_push_entry(
                            Default::default(),
                            id,
                            gix::index::entry::Flags::empty(),
                            mode,
                            rela.as_bstr(),
                        );
                        index.sort_entries();
                    }
                }
                _ => {
                    if let Ok(existing) = index.entry_index_by_path(rela.as_bstr()) {
                        index.remove_entry_at_index(existing);
                    }
                }
            }
        }
        index.write(Default::default()).map_err(GitError::msg)?;
        Ok(())
    }

    /// Commit the current index. The tree is rebuilt from the index through
    /// the tree editor; HEAD advances with a reflog entry.
    fn commit_create(&self, root: &Path, message: &str) -> GitResult<String> {
        let message = message.trim();
        if message.is_empty() {
            return Err(GitError::Message("commit message is empty".into()));
        }
        let repo = self.repo(root)?;
        let index = repo.index_or_empty().map_err(GitError::msg)?;
        if index.entries().is_empty() {
            return Err(GitError::Message("nothing to commit".into()));
        }

        // Build the tree: start from the empty tree and upsert every index
        // entry (correct for any shape; cost is linear in index size).
        let empty = gix::ObjectId::empty_tree(repo.object_hash());
        let mut editor = repo.edit_tree(empty).map_err(GitError::msg)?;
        for entry in index.entries() {
            let path = entry.path(&index);
            let kind = match entry.mode {
                gix::index::entry::Mode::SYMLINK => gix::objs::tree::EntryKind::Link,
                m if m == gix::index::entry::Mode::FILE_EXECUTABLE => {
                    gix::objs::tree::EntryKind::BlobExecutable
                }
                _ => gix::objs::tree::EntryKind::Blob,
            };
            editor.upsert(path, kind, entry.id).map_err(GitError::msg)?;
        }
        let tree_id = editor.write().map_err(GitError::msg)?.detach();

        // Refuse empty commits (tree identical to HEAD's).
        let parent = repo.head_id().ok().map(|id| id.detach());
        if let Some(parent_id) = parent
            && let Ok(parent_commit) = repo.find_commit(parent_id)
            && parent_commit.tree_id().map(|t| t.detach()).ok() == Some(tree_id)
        {
            return Err(GitError::Message("no staged changes to commit".into()));
        }

        // Identity: repo-local config first (isolated open); fall back to a
        // standard open *only* to read user.name/user.email from the global
        // config — that handle reads two values and is dropped immediately.
        let sig = self.resolve_identity(&repo, root)?;
        let mut time_buf1 = Default::default();
        let mut time_buf2 = Default::default();
        let id = repo
            .commit_as(
                sig.to_ref(&mut time_buf1),
                sig.to_ref(&mut time_buf2),
                "HEAD",
                message,
                tree_id,
                parent.into_iter(),
            )
            .map_err(GitError::msg)?;
        Ok(id.to_string())
    }

    /// Fetch from one configured remote over HTTPS only.
    ///
    /// Two-phase security posture:
    /// 1. The *isolated* handle pre-validates that the remote exists in the
    ///    repository's own config and that its fetch URL scheme is https.
    /// 2. A *standard* handle (able to see the user's credential-helper
    ///    configuration → Windows Credential Manager via GCM) performs the
    ///    network operation, re-validating the scheme in case environment
    ///    config rewrote the URL. The handle lives for this call only and no
    ///    hook/filter API is ever invoked on it. Credentials are never seen
    ///    or stored by PIKU — the helper chain supplies them to gix directly.
    fn fetch(
        &self,
        root: &Path,
        remote: &str,
        progress: &UnboundedSender<JobEvent>,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<FetchOutcome> {
        let sanitized_root = self.guard.sanitize(root).map_err(GitError::msg)?;

        // Phase 1: isolated validation.
        let iso = self.repo(root)?;
        let raw_name: BString = iso
            .remote_names()
            .iter()
            .find(|name| clean_ref(name.as_ref()) == remote)
            .map(|name| name.as_ref().to_owned())
            .ok_or_else(|| GitError::Message(format!("unknown remote `{remote}`")))?;
        let https_only = |repo: &gix::Repository| -> GitResult<()> {
            let remote_obj = repo
                .find_remote(raw_name.as_bstr())
                .map_err(GitError::msg)?;
            let url = remote_obj
                .url(gix::remote::Direction::Fetch)
                .ok_or_else(|| GitError::Message("remote has no fetch URL".into()))?;
            if !matches!(url.scheme, gix::url::Scheme::Https) {
                return Err(GitError::Message(
                    "only https remotes can be fetched (ssh/file/git are disabled)".into(),
                ));
            }
            Ok(())
        };
        https_only(&iso)?;
        drop(iso);

        // Phase 2: the actual fetch on a short-lived standard handle.
        let repo = gix::open(&sanitized_root).map_err(GitError::msg)?;
        https_only(&repo)?;
        let remote_obj = repo
            .find_remote(raw_name.as_bstr())
            .map_err(GitError::msg)?;

        let _ = progress.unbounded_send(JobEvent::Scanned {
            total_bytes: 0,
            total_items: 0,
        });
        let connection = remote_obj
            .connect(gix::remote::Direction::Fetch)
            .map_err(GitError::msg)?;
        let prepared = connection
            .prepare_fetch(gix::progress::Discard, Default::default())
            .map_err(GitError::msg)?;
        let outcome = prepared
            .receive(gix::progress::Discard, interrupt)
            .map_err(GitError::msg)?;
        if interrupt.load(Ordering::Relaxed) {
            return Err(GitError::Interrupted);
        }

        use gix::remote::fetch::Status;
        let updated_refs = match &outcome.status {
            Status::NoPackReceived { update_refs, .. } => update_refs.edits.len(),
            Status::Change { update_refs, .. } => update_refs.edits.len(),
        };
        let _ = progress.unbounded_send(JobEvent::Progress {
            delta_bytes: 0,
            delta_items: updated_refs,
        });
        Ok(FetchOutcome {
            remote: remote.to_string(),
            updated_refs,
        })
    }
}

impl GixBackend {
    /// Bytes of a blob object, capped.
    fn blob_bytes(
        &self,
        repo: &gix::Repository,
        id: gix::ObjectId,
        cap: usize,
    ) -> GitResult<(Vec<u8>, bool)> {
        let blob = repo.find_blob(id).map_err(GitError::msg)?;
        let truncated = blob.data.len() > cap;
        let mut bytes = blob.data.clone();
        bytes.truncate(cap);
        Ok((bytes, truncated))
    }

    /// The on-disk index as an owned, writable file — or a fresh empty one
    /// when the repository has none yet.
    fn open_or_new_index(&self, repo: &gix::Repository) -> GitResult<gix::index::File> {
        match repo.open_index() {
            Ok(index) => Ok(index),
            Err(_) => Ok(gix::index::File::from_state(
                gix::index::State::new(repo.object_hash()),
                repo.index_path(),
            )),
        }
    }

    /// Identity for reflog lines on ref edits: the user's real identity when
    /// configured, else a fixed application identity (reflogs are local
    /// metadata — failing the operation over a missing name helps nobody).
    fn reflog_identity(&self, repo: &gix::Repository, root: &Path) -> gix::actor::Signature {
        self.resolve_identity(repo, root)
            .unwrap_or_else(|_| gix::actor::Signature {
                name: "piku".into(),
                email: "piku@localhost".into(),
                time: gix::date::Time::now_utc(),
            })
    }

    /// Committer identity. The isolated handle only sees repo-local config;
    /// when `user.*` is not set there, a standard (non-isolated) handle is
    /// opened just long enough to read the global identity — nothing else is
    /// read from it and no repo-configured executable is ever invoked.
    fn resolve_identity(
        &self,
        repo: &gix::Repository,
        root: &Path,
    ) -> GitResult<gix::actor::Signature> {
        if let Some(Ok(sig)) = repo.committer() {
            return Ok(sig.into());
        }
        if let Ok(outer) = gix::open(root)
            && let Some(Ok(sig)) = outer.committer()
        {
            return Ok(sig.into());
        }
        Err(GitError::Message(
            "no git identity configured — set user.name and user.email".into(),
        ))
    }

    /// Bytes of a working-tree file, path re-validated, symlinks refused.
    fn worktree_bytes(
        &self,
        root: &Path,
        rel_path: &Path,
        cap: usize,
    ) -> GitResult<(Vec<u8>, bool)> {
        let abs = self
            .guard
            .sanitize(&root.join(rel_path))
            .map_err(GitError::msg)?;
        let meta = std::fs::symlink_metadata(&abs).map_err(GitError::msg)?;
        if !meta.is_file() {
            return Err(GitError::Message("not a regular file".into()));
        }
        let truncated = meta.len() > cap as u64;
        let mut bytes = std::fs::read(&abs).map_err(GitError::msg)?;
        bytes.truncate(cap);
        Ok((bytes, truncated))
    }
}

/// Repo-relative `Path` → slash-separated string for git APIs.
fn rel_to_slash(rel: &Path) -> GitResult<BString> {
    if rel.as_os_str().is_empty() || rel.is_absolute() {
        return Err(GitError::Message("invalid repository-relative path".into()));
    }
    let text = rel.to_string_lossy();
    if text.contains('\0') || text.split(['/', '\\']).any(|c| c == "..") {
        return Err(GitError::Message("invalid repository-relative path".into()));
    }
    Ok(BString::from(text.replace('\\', "/")))
}

/// Structural line diff via gix's bundled imara-diff: interned byte lines,
/// histogram algorithm, hunks with `DIFF_CONTEXT_LINES` of context. Returns
/// `(hunks, line_capped)`.
fn compute_hunks(old: &[u8], new: &[u8]) -> (Vec<DiffHunk>, bool) {
    use gix::diff::blob::sources::byte_lines;
    use gix::diff::blob::{Algorithm, Diff, InternedInput};

    let input = InternedInput::new(byte_lines(old), byte_lines(new));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let line_text = |token: gix::diff::blob::Token| -> String {
        let mut bytes: &[u8] = input.interner[token];
        // Tokens keep their line terminator; the renderer adds its own rows.
        while let [rest @ .., b'\n' | b'\r'] = bytes {
            bytes = rest;
        }
        sanitize_git_text(&String::from_utf8_lossy(bytes), DIFF_MAX_LINE_CHARS, false)
    };

    let mut hunks = Vec::new();
    let mut total_lines = 0usize;
    let mut capped = false;
    // Tracks how far context on the "before" side has been consumed so
    // adjacent hunks never emit overlapping context lines.
    let mut prev_before_end = 0usize;

    for hunk in diff.hunks() {
        if total_lines >= DIFF_MAX_LINES {
            capped = true;
            break;
        }
        let before = hunk.before.start as usize..hunk.before.end as usize;
        let after = hunk.after.start as usize..hunk.after.end as usize;

        let ctx_start = before
            .start
            .saturating_sub(DIFF_CONTEXT_LINES)
            .max(prev_before_end.min(before.start));
        let ctx_end = (before.end + DIFF_CONTEXT_LINES).min(input.before.len());
        prev_before_end = ctx_end;

        let mut lines = Vec::new();
        for ix in ctx_start..before.start {
            lines.push((DiffLineKind::Context, line_text(input.before[ix])));
        }
        for ix in before.clone() {
            lines.push((DiffLineKind::Del, line_text(input.before[ix])));
        }
        for ix in after.clone() {
            lines.push((DiffLineKind::Add, line_text(input.after[ix])));
        }
        for ix in before.end..ctx_end {
            lines.push((DiffLineKind::Context, line_text(input.before[ix])));
        }

        if total_lines + lines.len() > DIFF_MAX_LINES {
            lines.truncate(DIFF_MAX_LINES - total_lines);
            capped = true;
        }
        total_lines += lines.len();

        // 1-based unified-diff style header, context included.
        let old_start = ctx_start + 1;
        let old_count = (before.end - ctx_start) + (ctx_end - before.end);
        let new_start = after.start.saturating_sub(before.start - ctx_start) + 1;
        let new_count = after.len() + (before.start - ctx_start) + (ctx_end - before.end);
        hunks.push(DiffHunk {
            header: format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@"),
            lines,
        });
        if capped {
            break;
        }
    }
    (hunks, capped)
}
