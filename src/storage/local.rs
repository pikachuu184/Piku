//! Local filesystem provider. Every call sanitizes its paths through the
//! [`PathGuard`] before touching the disk, and every mutation is audited.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context as _, bail};

use crate::core::entry::FsEntry;
use crate::security::audit;
use crate::security::path_guard::PathGuard;
use crate::storage::provider::{ProgressFn, StorageProvider};

const COPY_CHUNK: usize = 1024 * 1024;

pub struct LocalProvider {
    guard: PathGuard,
}

impl LocalProvider {
    pub fn new() -> Self {
        Self {
            guard: PathGuard::with_system_roots(),
        }
    }

    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    /// Sanitize a path and open it read-only, returning the handle and the
    /// file's total size. For bounded preview parsers that need `Seek`
    /// (zip central directories, audio tag readers). Refuses symlinks and
    /// directories; callers must keep their own read caps.
    pub fn open_read(&self, path: &Path) -> anyhow::Result<(fs::File, u64)> {
        crate::app::diagnostics::assert_not_rendering("storage::open_read");
        let path = self.guard.sanitize(path)?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("reading metadata of {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("refusing to read through a link: {}", path.display());
        }
        if !metadata.is_file() {
            bail!("`{}` is not a regular file", path.display());
        }
        let file = fs::File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        Ok((file, metadata.len()))
    }
}

impl StorageProvider for LocalProvider {
    fn name(&self) -> &'static str {
        "local"
    }

    fn list(&self, dir: &Path) -> anyhow::Result<Vec<FsEntry>> {
        crate::app::diagnostics::assert_not_rendering("storage::list");
        let dir = self.guard.sanitize(dir)?;
        let mut entries = Vec::new();
        for item in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let Ok(item) = item else { continue };
            let path = item.path();
            // symlink_metadata so links are shown as links, never traversed.
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            entries.push(FsEntry::from_metadata(path, &metadata));
        }
        Ok(entries)
    }

    fn stat(&self, path: &Path) -> anyhow::Result<FsEntry> {
        crate::app::diagnostics::assert_not_rendering("storage::stat");
        let path = self.guard.sanitize(path)?;
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("reading metadata of {}", path.display()))?;
        Ok(FsEntry::from_metadata(path, &metadata))
    }

    fn create_dir(&self, path: &Path) -> anyhow::Result<()> {
        crate::app::diagnostics::assert_not_rendering("storage::create_dir");
        let path = self.guard.sanitize(path)?;
        // No `exists()` pre-check: `create_dir` itself fails atomically when
        // the target exists, so there is no TOCTOU window to race.
        let result = fs::create_dir(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("`{}` already exists", path.display())
            } else {
                anyhow::Error::new(error).context(format!("creating {}", path.display()))
            }
        });
        audit::record(
            "create_dir",
            &path,
            None,
            result.is_ok(),
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
                .as_str(),
        );
        result
    }

    fn create_file(&self, path: &Path) -> anyhow::Result<()> {
        crate::app::diagnostics::assert_not_rendering("storage::create_file");
        let path = self.guard.sanitize(path)?;
        // `create_new` makes the existence check atomic — no TOCTOU window.
        let result = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map(|_| ())
            .with_context(|| format!("creating {}", path.display()));
        audit::record(
            "create_file",
            &path,
            None,
            result.is_ok(),
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
                .as_str(),
        );
        result
    }

    fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
        crate::app::diagnostics::assert_not_rendering("storage::rename");
        let from = self.guard.sanitize(from)?;
        let to = self.guard.sanitize(to)?;
        // No `exists()` pre-check: Windows MoveFileEx (without
        // REPLACE_EXISTING) fails atomically when the target exists.
        let result = fs::rename(&from, &to).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                anyhow::anyhow!("`{}` already exists", to.display())
            } else {
                anyhow::Error::new(error).context(format!("renaming {}", from.display()))
            }
        });
        audit::record(
            "rename",
            &from,
            Some(&to),
            result.is_ok(),
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
                .as_str(),
        );
        result
    }

    fn copy_file(
        &self,
        from: &Path,
        to: &Path,
        progress: ProgressFn,
        cancel: &AtomicBool,
    ) -> anyhow::Result<u64> {
        let from = self.guard.sanitize(from)?;
        let to = self.guard.sanitize(to)?;

        // Refuse a symlinked source, matching `open_read`. Copying *through* a
        // link silently duplicates whatever it points at, which may be
        // somewhere the user never selected.
        let src_meta = fs::symlink_metadata(&from)
            .with_context(|| format!("reading metadata of {}", from.display()))?;
        if src_meta.file_type().is_symlink() {
            bail!("refusing to copy through a link: {}", from.display());
        }

        // Refuse a destination that already exists as a symlink. `File::create`
        // truncates and *follows* it, so a link pre-planted at the destination
        // would redirect the write anywhere the user can write — unlike
        // `create_file`, which uses `create_new`. The transfer engine picks a
        // non-colliding name before calling this, so an existing destination
        // is already the unusual case.
        match fs::symlink_metadata(&to) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("refusing to overwrite a link: {}", to.display());
            }
            Ok(_) | Err(_) => {}
        }

        let mut src =
            fs::File::open(&from).with_context(|| format!("opening {}", from.display()))?;
        let mut dst =
            fs::File::create(&to).with_context(|| format!("creating {}", to.display()))?;

        let mut buffer = vec![0u8; COPY_CHUNK];
        let mut copied: u64 = 0;
        loop {
            if cancel.load(Ordering::Relaxed) {
                drop(dst);
                let _ = fs::remove_file(&to);
                audit::record("copy_file", &from, Some(&to), false, "cancelled");
                bail!("cancelled");
            }
            let read = src.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            dst.write_all(&buffer[..read])?;
            copied += read as u64;
            progress(read as u64);
        }
        dst.flush()?;
        audit::record("copy_file", &from, Some(&to), true, "");
        Ok(copied)
    }

    fn delete_to_trash(&self, paths: &[PathBuf]) -> anyhow::Result<()> {
        crate::app::diagnostics::assert_not_rendering("storage::delete_to_trash");
        let mut sanitized = Vec::with_capacity(paths.len());
        for path in paths {
            sanitized.push(self.guard.sanitize(path)?);
        }
        let result = trash::delete_all(&sanitized).context("moving items to the recycle bin");
        for path in &sanitized {
            audit::record(
                "delete_to_trash",
                path,
                None,
                result.is_ok(),
                result
                    .as_ref()
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_default()
                    .as_str(),
            );
        }
        result
    }

    fn remove_after_move(&self, path: &Path) -> anyhow::Result<()> {
        crate::app::diagnostics::assert_not_rendering("storage::remove_after_move");
        let path = self.guard.sanitize(path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let result = if metadata.is_dir() {
            fs::remove_dir(&path).with_context(|| format!("removing {}", path.display()))
        } else {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
        };
        audit::record(
            "remove_after_move",
            &path,
            None,
            result.is_ok(),
            result
                .as_ref()
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
                .as_str(),
        );
        result
    }
}
