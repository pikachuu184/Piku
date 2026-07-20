pub mod components;
pub mod explorer;
pub mod inspector;
pub mod shell;
pub mod sidebar;
pub mod statusbar;
pub mod titlebar;
pub mod workspace_dialogs;

use gpui::{App, AppContext as _};
use gpui_component::dock::{PanelInfo, register_panel};

use crate::state::pane_state::PaneSession;
use crate::ui::explorer::ExplorerPanel;
use crate::ui::inspector::InspectorPanel;
use crate::ui::sidebar::NavPanel;

/// Register every panel constructor so saved dock layouts can be restored.
pub fn register_panels(cx: &mut App) {
    register_panel(cx, NavPanel::PANEL_NAME, |_, _, _, window, cx| {
        Box::new(cx.new(|cx| NavPanel::new(window, cx)))
    });

    register_panel(cx, InspectorPanel::PANEL_NAME, |_, _, _, window, cx| {
        Box::new(cx.new(|cx| InspectorPanel::new(window, cx)))
    });

    register_panel(cx, ExplorerPanel::PANEL_NAME, |_, _, info, window, cx| {
        let session = match info {
            PanelInfo::Panel(value) => serde_json::from_value::<PaneSession>(value.clone()).ok(),
            _ => None,
        };
        let session =
            session.unwrap_or_else(|| PaneSession::at(crate::services::fs_service::home_dir()));
        Box::new(cx.new(|cx| ExplorerPanel::from_session(session, window, cx)))
    });
}
