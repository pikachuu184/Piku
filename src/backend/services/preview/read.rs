//! Bounded, typed reads for preview providers.
//!
//! This replaces `LocalProvider::open_read` / `read_head` inside the engine.
//! Not because those were wrong — the policy is copied from them exactly — but
//! because they return `anyhow::Result`, and a service on the backend spine
//! owes its caller a typed error so `BackendError::is_cancelled` can tell a
//! superseded selection from a real failure.
//!
//! The refusals, and why each one is here:
//!
//! * **Symlinks** are refused before they are followed. A preview must not be
//!   a way to read a file the containment policy would not have allowed
//!   directly.
//! * **Non-regular files** are refused because opening a fifo or a device node
//!   blocks forever, and "the inspector hangs" is indistinguishable from a
//!   crash to the person using it.
//!
//! `preview_read_failures_match_the_storage_provider` in `backend/error.rs`
//! pins that these produce the same user-visible text the storage provider
//! does.

use std::fs;
use std::io::Read as _;
use std::path::Path;

use crate::backend::error::{FileError, PreviewError};

/// Open a validated path read-only, returning the handle and the file's total
/// size. For parsers that need `Seek` (zip central directories, audio tag
/// readers). Callers keep their own read caps.
///
/// Blocking. Must run inside `runtime.blocking(..)`.
pub fn open_read(path: &Path) -> Result<(fs::File, u64), PreviewError> {
    let text = || -> std::sync::Arc<str> { path.display().to_string().as_str().into() };

    let metadata = fs::symlink_metadata(path).map_err(|source| FileError::Io {
        op: "reading metadata of",
        path: text(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(FileError::IsSymlink(text()).into());
    }
    if !metadata.is_file() {
        return Err(FileError::NotRegular(text()).into());
    }
    let file = fs::File::open(path).map_err(|source| FileError::Io {
        op: "opening",
        path: text(),
        source,
    })?;
    Ok((file, metadata.len()))
}

/// Read at most `cap` bytes from the head of a file, returning the bytes and
/// the file's true total size.
///
/// The total is what lets a caller report "truncated" honestly: the bytes are
/// bounded, the size is not.
pub fn read_head(path: &Path, cap: usize) -> Result<(Vec<u8>, u64), PreviewError> {
    let (file, total) = open_read(path)?;
    let mut buf = Vec::with_capacity(cap.min(total as usize));
    file.take(cap as u64)
        .read_to_end(&mut buf)
        .map_err(|source| FileError::Io {
            op: "reading",
            path: path.display().to_string().as_str().into(),
            source,
        })?;
    Ok((buf, total))
}

/// The outcome of [`read_all_bounded`]: either the whole file, or a refusal
/// carrying the size that earned it.
pub enum BoundedRead {
    All(Vec<u8>),
    TooLarge { size: u64 },
}

/// Read a file **whole**, refusing it before allocating if it is past `max`.
///
/// The distinction from [`read_head`] is the order of operations, and it is the
/// whole point. `read_head` is for parsers that work on a prefix, so it reads
/// first and lets the caller judge the total afterwards. A parser that needs
/// the complete file cannot use a prefix, so reading one and then refusing it
/// allocates the cap for nothing — which for a 256 MiB cap is not nothing.
///
/// Blocking. Must run inside `runtime.blocking(..)`.
pub fn read_all_bounded(path: &Path, max: u64) -> Result<BoundedRead, PreviewError> {
    let (file, total) = open_read(path)?;
    if total > max {
        return Ok(BoundedRead::TooLarge { size: total });
    }
    // `total` is already known to be <= max, so this reserves the file's real
    // size rather than the budget.
    let mut buf = Vec::with_capacity(total as usize);
    file.take(max)
        .read_to_end(&mut buf)
        .map_err(|source| FileError::Io {
            op: "reading",
            path: path.display().to_string().as_str().into(),
            source,
        })?;
    Ok(BoundedRead::All(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join("piku-preview-read-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn read_head_bounds_the_bytes_but_reports_the_true_size() {
        let dir = scratch("head");
        let file = dir.join("big.txt");
        let _ = std::fs::write(&file, vec![b'a'; 10_000]);

        let (bytes, total) = read_head(&file, 100).expect("read");
        assert_eq!(bytes.len(), 100, "the cap must bound what is read");
        assert_eq!(total, 10_000, "the caller still needs the real size");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let dir = scratch("dir");
        assert!(matches!(
            open_read(&dir),
            Err(PreviewError::File(FileError::NotRegular(_)))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_reports_io_rather_than_panicking() {
        let dir = scratch("missing");
        assert!(matches!(
            open_read(&dir.join("nope.txt")),
            Err(PreviewError::File(FileError::Io { .. }))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A preview must never be a way to read through a link to a file the
    /// containment policy would have refused.
    #[cfg(unix)]
    #[test]
    fn a_symlink_is_refused_before_it_is_followed() {
        let dir = scratch("symlink");
        let target = dir.join("real.txt");
        let link = dir.join("link.txt");
        let _ = std::fs::write(&target, b"secret");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        assert!(matches!(
            open_read(&link),
            Err(PreviewError::File(FileError::IsSymlink(_)))
        ));
        // And the same through the capped reader, which is what providers call.
        assert!(matches!(
            read_head(&link, 16),
            Err(PreviewError::File(FileError::IsSymlink(_)))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
