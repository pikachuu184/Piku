//! Global gpui actions and key bindings.

use std::path::PathBuf;

use gpui::{Action, App, KeyBinding, actions};
use serde::Deserialize;

/// Toggle a path in the favorites list (dispatched from context menus).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = piku, no_json)]
pub struct ToggleFavoritePath(pub PathBuf);

/// Toggle a path in the pinned list (dispatched from context menus).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = piku, no_json)]
pub struct TogglePinnedPath(pub PathBuf);

/// Remove a path from recents (dispatched from context menus).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = piku, no_json)]
pub struct RemoveRecentPath(pub PathBuf);

/// Switch to the workspace with the given id (dispatched from the switcher).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = piku, no_json)]
pub struct SwitchWorkspace(pub String);

/// Key context set on every explorer pane root, so file-management shortcuts
/// never fight with text inputs.
pub const EXPLORER_CONTEXT: &str = "Explorer";

actions!(
    piku,
    [
        NewTab,
        SplitRight,
        SplitDown,
        ToggleLeftDock,
        ToggleRightDock,
        NavigateBack,
        NavigateForward,
        NavigateUp,
        RefreshPane,
        FocusFilter,
        CopySelection,
        CutSelection,
        PasteClipboard,
        DeleteSelection,
        RenameSelection,
        NewFolder,
        ToggleHidden,
        ToggleViewMode,
        OpenSelection,
        SelectAll,
        SelectNext,
        SelectPrev,
        SortByName,
        SortBySize,
        SortByModified,
        SortByType,
        FavoriteSelection,
        PinSelection,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        CreateWorkspace,
        RenameWorkspace,
        DuplicateWorkspace,
        DeleteWorkspace,
        ShowAbout,
    ]
);

pub fn init(cx: &mut App) {
    let explorer = Some(EXPLORER_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("ctrl-t", NewTab, None),
        KeyBinding::new("ctrl-shift-e", SplitRight, None),
        KeyBinding::new("ctrl-shift-o", SplitDown, None),
        KeyBinding::new("ctrl-b", ToggleLeftDock, None),
        KeyBinding::new("ctrl-alt-b", ToggleRightDock, None),
        KeyBinding::new("alt-left", NavigateBack, explorer),
        KeyBinding::new("alt-right", NavigateForward, explorer),
        KeyBinding::new("alt-up", NavigateUp, explorer),
        KeyBinding::new("backspace", NavigateUp, explorer),
        KeyBinding::new("f5", RefreshPane, explorer),
        KeyBinding::new("ctrl-f", FocusFilter, explorer),
        KeyBinding::new("ctrl-c", CopySelection, explorer),
        KeyBinding::new("ctrl-x", CutSelection, explorer),
        KeyBinding::new("ctrl-v", PasteClipboard, explorer),
        KeyBinding::new("delete", DeleteSelection, explorer),
        KeyBinding::new("f2", RenameSelection, explorer),
        KeyBinding::new("ctrl-shift-n", NewFolder, explorer),
        KeyBinding::new("ctrl-h", ToggleHidden, explorer),
        KeyBinding::new("ctrl-shift-v", ToggleViewMode, explorer),
        KeyBinding::new("enter", OpenSelection, explorer),
        KeyBinding::new("ctrl-a", SelectAll, explorer),
        KeyBinding::new("down", SelectNext, explorer),
        KeyBinding::new("up", SelectPrev, explorer),
        KeyBinding::new("ctrl-=", ZoomIn, explorer),
        KeyBinding::new("ctrl-shift-=", ZoomIn, explorer),
        KeyBinding::new("ctrl--", ZoomOut, explorer),
        KeyBinding::new("ctrl-0", ResetZoom, explorer),
    ]);
}
