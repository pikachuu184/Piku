//! Right-click menu for the explorer content area. Items dispatch global
//! actions that the focused pane handles.

use gpui::{Context, Window};
use gpui_component::{Icon, IconName, menu::PopupMenu};

use crate::app::actions::{
    CopySelection, CutSelection, DeleteSelection, FavoriteSelection, NewFolder, OpenSelection,
    PasteClipboard, PinSelection, RefreshPane, RenameSelection,
};
use crate::app::assets::PikuIcon;

pub(super) fn build(
    menu: PopupMenu,
    has_selection: bool,
    has_clipboard: bool,
    _window: &mut Window,
    _cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut menu = menu;

    if has_selection {
        menu = menu
            .menu_with_icon("Open", Icon::new(IconName::ExternalLink), Box::new(OpenSelection))
            .separator()
            .menu_with_icon("Copy", Icon::new(IconName::Copy), Box::new(CopySelection))
            .menu_with_icon("Cut", Icon::new(PikuIcon::Scissors), Box::new(CutSelection));
    }
    if has_clipboard {
        menu = menu.menu_with_icon(
            "Paste",
            Icon::new(PikuIcon::Clipboard),
            Box::new(PasteClipboard),
        );
    }
    if has_selection || has_clipboard {
        menu = menu.separator();
    }
    if has_selection {
        menu = menu
            .menu_with_icon("Rename", Icon::new(PikuIcon::Pencil), Box::new(RenameSelection))
            .menu_with_icon(
                "Move to Recycle Bin",
                Icon::new(PikuIcon::Trash),
                Box::new(DeleteSelection),
            )
            .separator()
            .menu_with_icon(
                "Add to favorites",
                Icon::new(IconName::Star),
                Box::new(FavoriteSelection),
            )
            .menu_with_icon("Pin", Icon::new(PikuIcon::Pin), Box::new(PinSelection))
            .separator();
    }

    menu.menu_with_icon(
        "New folder",
        Icon::new(PikuIcon::FolderPlus),
        Box::new(NewFolder),
    )
    .menu_with_icon(
        "Refresh",
        Icon::new(PikuIcon::RefreshCw),
        Box::new(RefreshPane),
    )
}
