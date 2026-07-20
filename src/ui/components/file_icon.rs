//! Maps a file entry to its monochrome icon.

use gpui::{App, Styled as _};
use gpui_component::{ActiveTheme as _, Icon, IconName};

use crate::app::assets::PikuIcon;
use crate::core::entry::FsEntry;
use crate::core::file_type::{FileCategory, categorize};

pub fn category_icon(category: FileCategory) -> Icon {
    match category {
        FileCategory::Folder => Icon::new(IconName::Folder),
        FileCategory::Symlink => Icon::new(IconName::ExternalLink),
        FileCategory::Image => Icon::new(PikuIcon::Image),
        FileCategory::Video => Icon::new(PikuIcon::Film),
        FileCategory::Audio => Icon::new(PikuIcon::Music),
        FileCategory::Archive => Icon::new(PikuIcon::FileArchive),
        FileCategory::Document => Icon::new(PikuIcon::FileText),
        FileCategory::Code => Icon::new(PikuIcon::FileCode),
        FileCategory::Executable => Icon::new(IconName::SquareTerminal),
        FileCategory::Other => Icon::new(IconName::File),
    }
}

/// Icon with the monochrome hierarchy applied: folders read brighter than
/// files so structure stands out without any hue.
pub fn entry_icon(entry: &FsEntry, cx: &App) -> Icon {
    let category = categorize(entry);
    let color = if entry.is_dir() {
        cx.theme().foreground
    } else {
        cx.theme().muted_foreground
    };
    category_icon(category).text_color(color)
}
