mod context_menu;
mod dialogs;
mod explorer_panel;
mod file_grid;
mod file_list;
mod toolbar;

pub use explorer_panel::ExplorerPanel;

use std::path::PathBuf;

use gpui::{App, Window};

use crate::state::PikuState;

/// Navigate whichever explorer pane is currently active. Used by the sidebar
/// so navigation never spawns extra windows or panes.
pub fn navigate_active(path: PathBuf, window: &mut Window, cx: &mut App) {
    if let Some(weak) = PikuState::global(cx).active_explorer()
        && let Some(panel) = weak.upgrade() {
            panel.update(cx, |panel, cx| {
                panel.navigate_to(path, window, cx);
            });
        }
}
