//! Maps a file entry to its monochrome icon (or a live thumbnail).

use gpui::{
    AnyElement, App, Context, ImageSource, IntoElement, ObjectFit, ParentElement as _, Styled as _,
    StyledImage as _, div, img, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName};

use crate::app::assets::PikuIcon;
use crate::core::entry::FsEntry;
use crate::core::file_type::{FileCategory, categorize};
use crate::services::thumbnails::THUMB_TARGET;
use crate::state::PikuState;

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

/// The visual for a grid tile / list row: a decoded thumbnail when one is
/// available for an image, otherwise the monochrome category glyph. `box_px`
/// is the square edge the visual occupies. Requesting a decode is idempotent,
/// and the panel re-renders (via its observe on the cache) when it lands.
pub fn entry_visual<V: 'static>(entry: &FsEntry, box_px: f32, cx: &mut Context<V>) -> AnyElement {
    // Raster images decode directly; videos get an ffmpeg poster frame (which
    // silently falls back to the glyph when ffmpeg is unavailable). SVGs are
    // drawn by gpui itself and the `image` crate cannot size them, so they keep
    // the glyph.
    let thumbnailable = match categorize(entry) {
        FileCategory::Image => entry.ext != "svg",
        FileCategory::Video => true,
        _ => false,
    };
    if thumbnailable {
        let cache = PikuState::global(cx).thumbnails.clone();
        let ready = cache.update(cx, |cache, cx| {
            cache.request(entry, THUMB_TARGET, cx);
            cache.get(entry, THUMB_TARGET)
        });
        if let Some(image) = ready {
            let radius = cx.theme().radius;
            return div()
                .w(px(box_px))
                .h(px(box_px))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(image))
                        .max_w(px(box_px))
                        .max_h(px(box_px))
                        .object_fit(ObjectFit::Contain)
                        .rounded(radius),
                )
                .into_any_element();
        }
    }
    entry_icon(entry, cx).size(px(box_px)).into_any_element()
}
