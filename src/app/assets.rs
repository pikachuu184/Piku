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
    Logo,
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
    FilePlus,
    Columns2,
    Rows2,
    Layers,
    // Preview / media controls.
    Play,
    Pause,
    Square,
    Rewind,
    FastForward,
    Volume2,
    VolumeX,
    Maximize,
    Scan,
    ZoomIn,
    ZoomOut,
    Eye,
    ListTree,
    Braces,
    ExternalLink,
    Maximize2,
    // Video-player controls.
    StepBack,
    StepForward,
    Camera,
    Gauge,
    Minimize,
    // Git.
    GitBranch,
    GitCommit,
}

impl IconNamed for PikuIcon {
    fn path(self) -> SharedString {
        match self {
            Self::Logo => "icons/piku/logo.svg",
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
            Self::FilePlus => "icons/piku/file-plus.svg",
            Self::Columns2 => "icons/piku/columns-2.svg",
            Self::Rows2 => "icons/piku/rows-2.svg",
            Self::Layers => "icons/piku/layers.svg",
            Self::Play => "icons/piku/play.svg",
            Self::Pause => "icons/piku/pause.svg",
            Self::Square => "icons/piku/square.svg",
            Self::Rewind => "icons/piku/rewind.svg",
            Self::FastForward => "icons/piku/fast-forward.svg",
            Self::Volume2 => "icons/piku/volume-2.svg",
            Self::VolumeX => "icons/piku/volume-x.svg",
            Self::Maximize => "icons/piku/maximize.svg",
            Self::Scan => "icons/piku/scan.svg",
            Self::ZoomIn => "icons/piku/zoom-in.svg",
            Self::ZoomOut => "icons/piku/zoom-out.svg",
            Self::Eye => "icons/piku/eye.svg",
            Self::ListTree => "icons/piku/list-tree.svg",
            Self::Braces => "icons/piku/braces.svg",
            Self::ExternalLink => "icons/piku/external-link.svg",
            Self::Maximize2 => "icons/piku/maximize-2.svg",
            Self::StepBack => "icons/piku/step-back.svg",
            Self::StepForward => "icons/piku/step-forward.svg",
            Self::Camera => "icons/piku/camera.svg",
            Self::Gauge => "icons/piku/gauge.svg",
            Self::Minimize => "icons/piku/minimize.svg",
            Self::GitBranch => "icons/piku/git-branch.svg",
            Self::GitCommit => "icons/piku/git-commit.svg",
        }
        .into()
    }
}
