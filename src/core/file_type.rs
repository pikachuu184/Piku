//! Extension → category mapping. The UI layer maps categories to icons.

use crate::core::entry::{EntryKind, FsEntry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCategory {
    Folder,
    Symlink,
    Image,
    Video,
    Audio,
    Archive,
    Document,
    Code,
    Executable,
    Other,
}

impl FileCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Folder => "Folder",
            Self::Symlink => "Symbolic link",
            Self::Image => "Image",
            Self::Video => "Video",
            Self::Audio => "Audio",
            Self::Archive => "Archive",
            Self::Document => "Document",
            Self::Code => "Source code",
            Self::Executable => "Application",
            Self::Other => "File",
        }
    }
}

pub fn categorize(entry: &FsEntry) -> FileCategory {
    match entry.kind {
        EntryKind::Directory => FileCategory::Folder,
        EntryKind::Symlink => FileCategory::Symlink,
        EntryKind::File => categorize_ext(&entry.ext),
    }
}

pub fn categorize_ext(ext: &str) -> FileCategory {
    match ext {
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "svg" | "tiff" | "tif"
        | "avif" | "heic" => FileCategory::Image,
        "mp4" | "mkv" | "avi" | "mov" | "wmv" | "webm" | "flv" | "m4v" | "mpg" | "mpeg" => {
            FileCategory::Video
        }
        "mp3" | "wav" | "flac" | "ogg" | "aac" | "m4a" | "wma" | "opus" | "mid" => {
            FileCategory::Audio
        }
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" | "iso" | "cab" => {
            FileCategory::Archive
        }
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
        | "txt" | "md" | "rtf" | "csv" | "log" | "epub" => FileCategory::Document,
        "rs" | "c" | "h" | "cpp" | "hpp" | "cc" | "cs" | "java" | "kt" | "go" | "py" | "rb"
        | "js" | "jsx" | "ts" | "tsx" | "json" | "toml" | "yaml" | "yml" | "xml" | "html"
        | "css" | "scss" | "sh" | "ps1" | "bat" | "cmd" | "sql" | "php" | "swift" | "lua"
        | "zig" | "vue" | "svelte" | "ini" | "cfg" | "conf" | "lock" => FileCategory::Code,
        "exe" | "msi" | "dll" | "com" | "appx" | "app" | "deb" | "rpm" | "apk" => {
            FileCategory::Executable
        }
        _ => FileCategory::Other,
    }
}

// Per-format preview gating now lives in `crate::preview::decide_kind` —
// every file gets some preview (hex at worst), so the old
// `is_*_previewable` predicates are gone.

/// Extensions the Windows shell will *execute* rather than display when
/// opened with the default association. Shell-open of any of these asks for
/// confirmation first — this is the app's largest code-execution surface.
/// (`.js` is source code in the listing, but double-clicking it runs it via
/// Windows Script Host, which is exactly the risk.)
pub const RISKY_OPEN_EXTS: &[&str] = &[
    "exe", "bat", "cmd", "com", "scr", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh",
    "msi", "msp", "hta", "pif", "lnk", "cpl", "jar", "reg",
];
