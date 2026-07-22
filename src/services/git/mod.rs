//! Git as a workspace service. Repositories are discovered passively while
//! the user navigates, monitored through `.git` watchers, and surfaced as
//! plain sanitized data the UI observes — gix types never cross into render
//! code, and no hook, filter, or external driver is ever executed.

pub mod backend;
pub mod gix_backend;
pub mod store;
pub mod types;

/// Status entries reported per repo before the snapshot marks itself
/// truncated — keeps pathological working trees from exhausting memory.
pub const STATUS_MAX_ENTRIES: usize = 5_000;
/// Ignored entries tracked per repo (collapsed to directories, so `target/`
/// is one entry). Separate budget — ignored decorations must never starve
/// the dirty-entry map.
pub const IGNORED_MAX_ENTRIES: usize = 2_000;
/// Commits fetched per history page ("Load more" walks further).
pub const COMMIT_PAGE: usize = 100;
/// Commits scanned when building a single file's history before giving up.
pub const FILE_HISTORY_WALK_CAP: usize = 2_000;
/// Ahead/behind counting stops here and reports "cap+".
pub const AHEAD_BEHIND_CAP: usize = 1_000;
/// Per-side byte cap for text diffs; larger blobs report "too large".
pub const DIFF_MAX_BYTES: u64 = 512 * 1024;
/// Byte cap when materializing a historical blob for preview (used by the
/// `blob_at` backend path; UI wiring lands with historical previews).
#[allow(dead_code)]
pub const BLOB_PREVIEW_CAP: usize = 256 * 1024;
/// Debounce window for `.git` watcher events (index.lock churn protection).
pub const GIT_DEBOUNCE_MS: u64 = 500;
/// Character caps for repo-derived strings (see `security::git_text`).
pub const MAX_REF_CHARS: usize = 200;
pub const MAX_SUMMARY_CHARS: usize = 300;
pub const MAX_BODY_CHARS: usize = 10_000;
pub const MAX_AUTHOR_CHARS: usize = 120;
pub const MAX_URL_CHARS: usize = 300;
