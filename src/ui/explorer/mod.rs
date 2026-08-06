mod context_menu;
mod dialogs;
mod explorer_panel;
mod file_grid;
mod file_list;
mod toolbar;

pub use explorer_panel::ExplorerPanel;
/// Hand a path to the OS default handler, canonicalizing and re-authorizing
/// the resolved target first. Exported so the inspector and media panel use
/// this one hardened path rather than calling `open::that_detached` directly.
pub use explorer_panel::shell_open;

use std::path::PathBuf;

use gpui::{App, Window};

use crate::state::PikuState;

/// Navigate whichever explorer pane is currently active. Used by the sidebar
/// so navigation never spawns extra windows or panes.
pub fn navigate_active(path: PathBuf, window: &mut Window, cx: &mut App) {
    if let Some(weak) = PikuState::global(cx).active_explorer()
        && let Some(panel) = weak.upgrade()
    {
        panel.update(cx, |panel, cx| {
            panel.navigate_to(path, window, cx);
        });
    }
}
