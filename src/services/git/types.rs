//! Plain-data types crossing the git-backend → UI boundary. Every string in
//! here has already been through `security::git_text::sanitize_git_text` and
//! every path through `PathGuard::sanitize` — render code can trust them.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// One side of a file's status (index or worktree).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GitStatusCode {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
    Conflicted,
    /// Excluded by `.gitignore` rules — informational only, never counted
    /// as dirty and stored outside the dirty-entry map.
    Ignored,
}

impl GitStatusCode {
    /// Monochrome letter badge rendered in the file list.
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
            Self::Untracked => "U",
            Self::Conflicted => "!",
            Self::Ignored => "i",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "Modified",
            Self::Added => "Added",
            Self::Deleted => "Deleted",
            Self::Renamed => "Renamed",
            Self::Untracked => "Untracked",
            Self::Conflicted => "Conflicted",
            Self::Ignored => "Ignored",
        }
    }
}

/// Combined status of one file: what the index says vs. the working tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitFileStatus {
    pub index: Option<GitStatusCode>,
    pub worktree: Option<GitStatusCode>,
}

impl GitFileStatus {
    /// The single most relevant code for a compact one-glyph badge.
    pub fn primary(&self) -> Option<GitStatusCode> {
        if self.index == Some(GitStatusCode::Conflicted)
            || self.worktree == Some(GitStatusCode::Conflicted)
        {
            return Some(GitStatusCode::Conflicted);
        }
        self.worktree.or(self.index)
    }

    pub fn is_staged(&self) -> bool {
        self.index.is_some()
    }
}

/// A remote as shown in the inspector; `fetchable` means its URL passed the
/// https scheme allowlist.
#[derive(Debug, Clone)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
    pub fetchable: bool,
}

/// Repository-level summary: branch, sync state, cleanliness counts.
#[derive(Debug, Clone, Default)]
pub struct RepoSnapshot {
    /// Worktree root (PathGuard-sanitized absolute path). Kept so snapshot
    /// clones stay self-describing even away from the store's key.
    #[allow(dead_code)]
    pub root: PathBuf,
    /// Current branch short name, `None` when HEAD is detached or unborn.
    pub branch: Option<String>,
    /// Short commit id when HEAD is detached.
    pub detached_short: Option<String>,
    /// Commits ahead of / behind the tracked upstream. `None` = no upstream.
    /// Values are capped at `AHEAD_BEHIND_CAP`.
    pub ahead: Option<usize>,
    pub behind: Option<usize>,
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflicted: usize,
    pub remotes: Vec<RemoteInfo>,
    /// True when the status walk hit `STATUS_MAX_ENTRIES` and stopped.
    pub truncated: bool,
    /// In-progress multi-step operation ("merging", "rebasing", …) detected
    /// from the repository state; static strings, nothing to sanitize.
    pub in_progress: Option<&'static str>,
}

impl RepoSnapshot {
    pub fn dirty_total(&self) -> usize {
        self.staged + self.unstaged + self.untracked + self.conflicted
    }
}

/// Per-file status map for one repository, shared cheaply with render code.
#[derive(Debug, Clone, Default)]
pub struct StatusSnapshot {
    /// Absolute sanitized path → status.
    pub by_path: Arc<HashMap<PathBuf, GitFileStatus>>,
    /// Directories (absolute, sanitized) containing at least one dirty entry.
    pub dirty_dirs: Arc<HashSet<PathBuf>>,
    /// Ignored entries (files or collapsed directories), absolute sanitized
    /// paths. Separate from `by_path` so they never count as dirty.
    pub ignored: Arc<HashSet<PathBuf>>,
}

/// One commit in a history listing.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    /// Full hex object id (ASCII, safe by construction).
    pub id: String,
    /// Abbreviated id for display.
    pub short: String,
    /// First line of the message, sanitized + capped.
    pub summary: String,
    /// Author name, sanitized + capped.
    pub author: String,
    pub time: SystemTime,
    /// Branch/tag decorations pointing at this commit, sanitized.
    pub refs: Vec<String>,
}

/// A file touched by a commit.
#[derive(Debug, Clone)]
pub struct ChangedFile {
    /// Repo-relative path for display (sanitized as text).
    pub rel_path: String,
    /// Absolute path if it validates under the PathGuard, else `None`
    /// (deleted files or hostile paths never reach the filesystem). Reserved
    /// for "reveal in explorer" once commit changes gain a context menu.
    #[allow(dead_code)]
    pub abs_path: Option<PathBuf>,
    pub code: GitStatusCode,
}

/// Full detail for one selected commit.
#[derive(Debug, Clone)]
pub struct CommitDetail {
    #[allow(dead_code)]
    pub info: CommitInfo,
    /// Message body (without the summary line), sanitized, newlines kept.
    pub body: String,
    pub changes: Vec<ChangedFile>,
    /// True when the change list was capped.
    pub truncated: bool,
}

/// One branch row in the branch popover.
#[derive(Debug, Clone)]
pub struct BranchInfo {
    /// Short display name, sanitized (rendering only — may differ from the
    /// real ref for exotic branch names).
    pub name: String,
    /// Full reference name (`refs/heads/…`), operations-only and never
    /// rendered: checkout/delete use this so branches whose names contain
    /// non-ASCII characters remain operable.
    pub ref_name: String,
    pub is_head: bool,
    pub is_remote: bool,
    /// Upstream short name for local branches, sanitized.
    pub upstream: Option<String>,
}

/// What a diff request compares.
#[derive(Debug, Clone)]
pub enum DiffTarget {
    /// Working tree file vs. what the index holds.
    WorktreeVsIndex { rel_path: PathBuf },
    /// Index vs. HEAD (a staged change).
    IndexVsHead { rel_path: PathBuf },
    /// A commit vs. its first parent for one file.
    CommitVsParent { commit: String, rel_path: PathBuf },
}

/// Kind of one rendered diff line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// `@@ -a,b +c,d @@` style header, sanitized.
    pub header: String,
    pub lines: Vec<(DiffLineKind, String)>,
}

/// A computed textual diff ready for rendering.
#[derive(Debug, Clone)]
pub struct DiffPayload {
    pub old_label: String,
    pub new_label: String,
    pub hunks: Vec<DiffHunk>,
    /// True when either side exceeded `DIFF_MAX_BYTES` or was binary.
    pub truncated: bool,
    /// Set when no textual diff could be produced (binary/oversized).
    pub note: Option<String>,
}

/// Outcome of a fetch operation.
#[derive(Debug, Clone, Default)]
pub struct FetchOutcome {
    pub remote: String,
    /// Refs updated by the fetch.
    pub updated_refs: usize,
}
