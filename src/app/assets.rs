//! Asset source combining PIKU's own Lucide icons with the icon set bundled
//! by `gpui-component-assets`.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_component::IconNamed;

#[derive(rust_embed::RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*.svg"]
struct EmbeddedAssets;

pub struct PikuAssets;

impl AssetSource for PikuAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.is_empty() {
            return Ok(None);
        }
        if let Some(file) = EmbeddedAssets::get(path) {
            return Ok(Some(file.data));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut items: Vec<SharedString> = EmbeddedAssets::iter()
            .filter(|p| p.starts_with(path))
            .map(|p| SharedString::from(p.to_string()))
            .collect();
        items.extend(gpui_component_assets::Assets.list(path)?);
        Ok(items)
    }
}

/// Additional Lucide icons that the bundled asset crate does not ship.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PikuIcon {
    House,
    Clock,
    Pin,
    Image,
    Film,
    Music,
    FileArchive,
    FileText,
    FileCode,
    LayoutGrid,
    List,
    Scissors,
    Clipboard,
    Trash,
    RefreshCw,
    Pencil,
    Monitor,
    FolderPlus,
    Columns2,
    Rows2,
}

impl IconNamed for PikuIcon {
    fn path(self) -> SharedString {
        match self {
            Self::House => "icons/piku/house.svg",
            Self::Clock => "icons/piku/clock.svg",
            Self::Pin => "icons/piku/pin.svg",
            Self::Image => "icons/piku/image.svg",
            Self::Film => "icons/piku/film.svg",
            Self::Music => "icons/piku/music.svg",
            Self::FileArchive => "icons/piku/file-archive.svg",
            Self::FileText => "icons/piku/file-text.svg",
            Self::FileCode => "icons/piku/file-code.svg",
            Self::LayoutGrid => "icons/piku/layout-grid.svg",
            Self::List => "icons/piku/list.svg",
            Self::Scissors => "icons/piku/scissors.svg",
            Self::Clipboard => "icons/piku/clipboard.svg",
            Self::Trash => "icons/piku/trash-2.svg",
            Self::RefreshCw => "icons/piku/refresh-cw.svg",
            Self::Pencil => "icons/piku/pencil.svg",
            Self::Monitor => "icons/piku/monitor.svg",
            Self::FolderPlus => "icons/piku/folder-plus.svg",
            Self::Columns2 => "icons/piku/columns-2.svg",
            Self::Rows2 => "icons/piku/rows-2.svg",
        }
        .into()
    }
}
