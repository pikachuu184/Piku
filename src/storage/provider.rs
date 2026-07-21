//! The storage abstraction every UI operation goes through. Implementations
//! are synchronous and blocking; callers run them on the background executor
//! so the render thread never touches storage.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::core::entry::FsEntry;

/// Byte-level progress callback used during long copies.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64);

pub trait StorageProvider: Send + Sync + 'static {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// List the immediate children of a directory.
    fn list(&self, dir: &Path) -> anyhow::Result<Vec<FsEntry>>;

    /// Metadata for a single path.
    #[allow(dead_code)]
    fn stat(&self, path: &Path) -> anyhow::Result<FsEntry>;

    /// Read at most `max` bytes from the start of a regular file, returning
    /// the bytes and the file's total size. Refuses symlinks and directories.
    /// Reads are not audited — the audit log is deliberately mutation-only.
    fn read_head(&self, path: &Path, max: usize) -> anyhow::Result<(Vec<u8>, u64)>;

    fn create_dir(&self, path: &Path) -> anyhow::Result<()>;

    /// Create a new empty file; fails if the path already exists.
    fn create_file(&self, path: &Path) -> anyhow::Result<()>;

    fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()>;

    /// Copy one file, reporting bytes copied and honouring cancellation.
    fn copy_file(
        &self,
        from: &Path,
        to: &Path,
        progress: ProgressFn,
        cancel: &AtomicBool,
    ) -> anyhow::Result<u64>;

    /// Move deleted items to the OS trash. PIKU never permanently deletes
    /// through the UI.
    fn delete_to_trash(&self, paths: &[PathBuf]) -> anyhow::Result<()>;

    /// Remove a file or empty directory permanently. Only the transfer engine
    /// uses this, to clear sources after a verified move.
    fn remove_after_move(&self, path: &Path) -> anyhow::Result<()>;
}
