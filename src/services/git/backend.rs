//! The service boundary between the UI and any concrete git implementation.
//! All methods are synchronous and blocking — callers run them on the
//! background executor. Everything returned is plain sanitized data
//! (`types.rs`); no gix type escapes the implementation.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::channel::mpsc::UnboundedSender;

use crate::services::jobs::JobEvent;

use super::types::{
    BranchInfo, CommitDetail, CommitInfo, DiffPayload, DiffTarget, FetchOutcome, RepoSnapshot,
    StatusSnapshot,
};

/// Errors surfaced to the UI as toast/inline text (already display-safe).
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("{0}")]
    Message(String),
    #[error("operation was interrupted")]
    Interrupted,
    #[error("repository is dirty — commit or discard changes first")]
    DirtyWorktree,
}

impl GitError {
    /// Wrap an implementation error. The text is sanitized before display —
    /// library errors can embed repo-derived strings.
    pub fn msg(err: impl std::fmt::Display) -> Self {
        Self::Message(crate::security::git_text::sanitize_git_text(
            &err.to_string(),
            300,
            false,
        ))
    }
}

pub type GitResult<T> = Result<T, GitError>;

/// Abstract git backend. Object-safe so the store can hold `Arc<dyn …>` and a
/// future implementation (or a test double) can swap in without UI changes.
pub trait GitBackend: Send + Sync {
    /// Find the repository containing `dir`, if any, returning its worktree
    /// root. Must never walk above the authorized storage root of `dir`.
    fn discover(&self, dir: &Path) -> Option<PathBuf>;

    /// Repository-level summary (branch, ahead/behind, cleanliness counts).
    fn snapshot(&self, root: &Path, interrupt: &Arc<AtomicBool>) -> GitResult<RepoSnapshot>;

    /// Per-file status map, capped at `STATUS_MAX_ENTRIES`.
    fn status(&self, root: &Path, interrupt: &Arc<AtomicBool>) -> GitResult<StatusSnapshot>;

    /// One page of commit history starting at HEAD, or continuing after
    /// `before` (exclusive) when paging.
    fn commits(
        &self,
        root: &Path,
        before: Option<&str>,
        page: usize,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<Vec<CommitInfo>>;

    /// Full detail (message body + changed files) for one commit.
    fn commit_detail(&self, root: &Path, id: &str, interrupt: &Arc<AtomicBool>)
    -> GitResult<CommitDetail>;

    /// History of one file (commits that touched it), bounded by
    /// `FILE_HISTORY_WALK_CAP` commits scanned.
    fn file_history(
        &self,
        root: &Path,
        rel_path: &Path,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<Vec<CommitInfo>>;

    /// Textual diff for one file between two states.
    fn diff_file(&self, root: &Path, target: &DiffTarget, interrupt: &Arc<AtomicBool>)
    -> GitResult<DiffPayload>;

    /// Raw bytes of `rel_path` as of `commit`, capped at `cap` bytes.
    /// Returns `(bytes, truncated)`. Not yet surfaced in the UI (historical
    /// blob previews are the next milestone); exercised by the test suite.
    #[allow(dead_code)]
    fn blob_at(
        &self,
        root: &Path,
        commit: &str,
        rel_path: &Path,
        cap: usize,
    ) -> GitResult<(Vec<u8>, bool)>;

    /// Local + remote branches.
    fn branches(&self, root: &Path) -> GitResult<Vec<BranchInfo>>;

    /// Check out an existing local branch. Refuses when the worktree is
    /// dirty (v1 policy — no auto-stash).
    fn checkout(&self, root: &Path, branch: &str, interrupt: &Arc<AtomicBool>) -> GitResult<()>;

    /// Create a local branch at HEAD (name pre-validated by the caller via
    /// `git_text::validate_branch_name`).
    fn create_branch(&self, root: &Path, name: &str) -> GitResult<()>;

    /// Delete a local branch (never the checked-out one).
    fn delete_branch(&self, root: &Path, name: &str) -> GitResult<()>;

    /// Stage the given repo-relative paths.
    fn stage(&self, root: &Path, rel_paths: &[PathBuf]) -> GitResult<()>;

    /// Unstage the given repo-relative paths (restore index from HEAD).
    fn unstage(&self, root: &Path, rel_paths: &[PathBuf]) -> GitResult<()>;

    /// Create a commit from the current index. Returns the new commit id.
    fn commit_create(&self, root: &Path, message: &str) -> GitResult<String>;

    /// Fetch from a configured remote (https only). Progress is streamed as
    /// `JobEvent`s; `interrupt` is the job's cancel flag.
    fn fetch(
        &self,
        root: &Path,
        remote: &str,
        progress: &UnboundedSender<JobEvent>,
        interrupt: &Arc<AtomicBool>,
    ) -> GitResult<FetchOutcome>;

    /// Whether push is available. The UI renders a disabled affordance when
    /// false; gix does not implement push yet.
    fn push_supported(&self) -> bool {
        false
    }
}
