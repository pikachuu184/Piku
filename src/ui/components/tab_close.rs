//! The little "X" that closes a tab.
//!
//! The tab strip is rendered by the vendored `gpui-component` `TabPanel`, whose
//! per-tab render draws no close control — it only wires an ellipsis dropdown.
//! We cannot edit that crate, but its render *does* call `Panel::title()` for
//! the tab label (when `tab_name` returns `None`), and `title()` returns an
//! arbitrary element. So each panel embeds this button in its `title()` to get
//! a one-click close next to the tab name.

use std::sync::Arc;

use gpui::{ElementId, IntoElement, WeakEntity, Window};
use gpui_component::{
    IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    dock::{PanelView, TabPanel},
};

/// A small ghost "X" that removes `panel` from its containing `tab_panel`.
///
/// Closing goes straight through the owning `TabPanel::remove_panel`, so it
/// targets this exact tab regardless of which tab is active. Call this only for
/// panels that are `closable()` — pinned tabs should get no button.
pub fn tab_close_button(
    id: impl Into<ElementId>,
    tab_panel: Option<WeakEntity<TabPanel>>,
    panel: Arc<dyn PanelView>,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(IconName::Close)
        .tooltip("Close tab")
        .on_click(move |_, window: &mut Window, cx| {
            // Don't also trigger the tab's own click (which would activate it)
            // or start a drag.
            cx.stop_propagation();
            if let Some(tab_panel) = tab_panel.as_ref().and_then(|handle| handle.upgrade()) {
                let panel = panel.clone();
                tab_panel.update(cx, |tab_panel, cx| {
                    tab_panel.remove_panel(panel, window, cx);
                });
            }
        })
}
