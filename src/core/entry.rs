//! The filesystem entry model shared by every layer.

use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Debug)]
pub struct FsEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub created: Option<SystemTime>,
    pub hidden: bool,
    pub readonly: bool,
    /// Lowercased extension without the leading dot; empty for directories.
    pub ext: String,
}

impl FsEntry {
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Directory
    }

    pub fn from_metadata(path: PathBuf, metadata: &std::fs::Metadata) -> Self {
        // Sanitized once, here, rather than at each of the dozen places a
        // name is rendered (list rows, grid tiles, breadcrumbs, tab titles,
        // the inspector, and — most importantly — the "this is a program, run
        // it?" dialog). A filename is attacker-controlled: without this, a
        // `U+202E` in one reverses everything after it, so `invoice<RLO>gpj.exe`
        // displays as `invoice.exe.jpg`.
        //
        // `path` keeps the real bytes; only the display name is cleaned.
        let raw = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let name = crate::security::text::sanitize_label(&raw);

        let kind = if metadata.is_symlink() {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else {
            EntryKind::File
        };

        let ext = if kind == EntryKind::Directory {
            String::new()
        } else {
            path.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        };

        let hidden = is_hidden(&name, metadata);

        Self {
            name,
            path,
            kind,
            size: if kind == EntryKind::File {
                metadata.len()
            } else {
                0
            },
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
            hidden,
            readonly: metadata.permissions().readonly(),
            ext,
        }
    }
}

#[cfg(windows)]
fn is_hidden(name: &str, metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    name.starts_with('.') || (metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN) != 0
}

#[cfg(not(windows))]
fn is_hidden(name: &str, _metadata: &std::fs::Metadata) -> bool {
    name.starts_with('.')
}
