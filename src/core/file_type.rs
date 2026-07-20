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

/// True when the file is small structured text that is safe to preview.
pub fn is_text_previewable(entry: &FsEntry) -> bool {
    entry.kind == EntryKind::File
        && entry.size <= 512 * 1024
        && matches!(
            categorize_ext(&entry.ext),
            FileCategory::Code | FileCategory::Document
        )
        && !matches!(
            entry.ext.as_str(),
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
                | "epub"
        )
}

pub fn is_image_previewable(entry: &FsEntry) -> bool {
    entry.kind == EntryKind::File
        && matches!(
            entry.ext.as_str(),
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "svg"
        )
}
