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

/// Open a media file in a dedicated, dockable media panel (dispatched from the
/// inspector preview and the bottom playback bar).
#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = piku, no_json)]
pub struct OpenMediaPanel(pub PathBuf);

/// Key context set on every explorer pane root, so file-management shortcuts
/// never fight with text inputs.
pub const EXPLORER_CONTEXT: &str = "Explorer";

/// Key context set on the inspector panel's root.
///
/// The preview shortcuts below are bound with no context on purpose: gpui
/// resolves actions along the focus path, and while browsing it is the file list
/// — a sibling of the inspector — that holds focus, so a context-scoped binding
/// would only fire after clicking into the dock. This context is for the
/// bindings that *should* need the panel to have focus, which is why the only
/// ones in it are the bare arrow keys.
pub const INSPECTOR_CONTEXT: &str = "Inspector";

actions!(
    piku,
    [
        NewTab,
        DuplicateTab,
        PinTab,
        SplitRight,
        SplitDown,
        ToggleLeftDock,
        ToggleRightDock,
        RevealPreview,
        NavigateBack,
        NavigateForward,
        NavigateUp,
        RefreshPane,
        FocusFilter,
        CopySelection,
        CutSelection,
        PasteClipboard,
        DeleteSelection,
        DeletePermanentSelection,
        RenameSelection,
        NewFolder,
        NewFile,
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
        PreviewZoomIn,
        PreviewZoomOut,
        PreviewResetView,
        PreviewFit,
        PreviewFitWidth,
        PreviewActualSize,
        PreviewRotate,
        PreviewNextPage,
        PreviewPrevPage,
        CreateWorkspace,
        RenameWorkspace,
        DuplicateWorkspace,
        DeleteWorkspace,
        ShowAbout,
        ToggleTransferCenter,
    ]
);

pub fn init(cx: &mut App) {
    let explorer = Some(EXPLORER_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("ctrl-t", NewTab, None),
        KeyBinding::new("ctrl-shift-d", DuplicateTab, None),
        // `ctrl-\` / `ctrl-shift-\` (VS Code style): the previous
        // `ctrl-shift-e` collided with the Input context's SelectToEndOfLine.
        KeyBinding::new("ctrl-\\", SplitRight, None),
        KeyBinding::new("ctrl-shift-\\", SplitDown, None),
        KeyBinding::new("ctrl-b", ToggleLeftDock, None),
        KeyBinding::new("ctrl-alt-b", ToggleRightDock, None),
        // Transfers are app-wide infrastructure, not a pane's business: a copy
        // outlives the tab that started it, so this must work from anywhere,
        // including from inside the inspector or the filter box. Hence context
        // `None`, like the dock toggles above.
        KeyBinding::new("ctrl-shift-t", ToggleTransferCenter, None),
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
        // The conventional shortcut for the irreversible one, and deliberately
        // one key away from the reversible one — which is why it always
        // confirms, whatever `confirm_delete` is set to.
        KeyBinding::new("shift-delete", DeletePermanentSelection, explorer),
        KeyBinding::new("f2", RenameSelection, explorer),
        KeyBinding::new("ctrl-shift-n", NewFolder, explorer),
        KeyBinding::new("ctrl-n", NewFile, explorer),
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
        // Preview transform and navigation. `ctrl-alt-` because the file grid
        // already owns `ctrl-=` / `ctrl--` / `ctrl-0` for icon size, and because
        // it matches `ctrl-alt-b` for the dock these act on. Context `None` for
        // the same reason as that one: they must work while the file list keeps
        // focus, which is where you are when you are looking at a preview.
        KeyBinding::new("ctrl-alt-=", PreviewZoomIn, None),
        KeyBinding::new("ctrl-alt-shift-=", PreviewZoomIn, None),
        KeyBinding::new("ctrl-alt--", PreviewZoomOut, None),
        KeyBinding::new("ctrl-alt-0", PreviewResetView, None),
        KeyBinding::new("ctrl-alt-9", PreviewFit, None),
        KeyBinding::new("ctrl-alt-1", PreviewActualSize, None),
        KeyBinding::new("ctrl-alt-w", PreviewFitWidth, None),
        KeyBinding::new("ctrl-alt-r", PreviewRotate, None),
        KeyBinding::new("ctrl-alt-right", PreviewNextPage, None),
        KeyBinding::new("ctrl-alt-left", PreviewPrevPage, None),
        // Bare arrows page a document when the inspector itself has focus, the
        // way they do in every viewer. `!Input` because the panel contains the
        // read-only code editor, and gpui's `Not` fails if the inner predicate
        // matches at *any* depth of the stack — so this cannot fire from inside
        // an input.
        KeyBinding::new("right", PreviewNextPage, Some(INSPECTOR_NOT_INPUT)),
        KeyBinding::new("left", PreviewPrevPage, Some(INSPECTOR_NOT_INPUT)),
        // Space reveals the preview for the selected file — the quick-look
        // gesture. It needs the `!Input` guard where `enter` and `backspace` do
        // not: those are bound by the Input context itself at a deeper node and
        // win the match there, while nothing binds `space`, so without the guard
        // it would fire from the filter box instead of typing a space.
        KeyBinding::new("space", RevealPreview, Some(EXPLORER_NOT_INPUT)),
    ]);
}

/// The inspector, but never from inside a text input. See the `space` binding.
const INSPECTOR_NOT_INPUT: &str = "Inspector && !Input";
/// The explorer, but never from inside a text input.
const EXPLORER_NOT_INPUT: &str = "Explorer && !Input";
