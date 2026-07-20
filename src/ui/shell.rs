//! The workspace shell: title bar, three-region dock area, status bar.
//! Layout is persisted as a logical dock graph and restored at startup.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Context, Edges, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Placement, Root, WindowExt as _,
    dock::{DockArea, DockAreaState, DockEvent, DockItem, DockPlacement},
    notification::Notification,
};

use crate::app::actions::{
    CreateWorkspace, DeleteWorkspace, DuplicateWorkspace, NewTab, RemoveRecentPath,
    RenameWorkspace, ShowAbout, SplitDown, SplitRight, SwitchWorkspace, ToggleFavoritePath,
    ToggleLeftDock, TogglePinnedPath, ToggleRightDock,
};
use crate::state::PikuState;
use crate::state::nav_model::NavModel;
use crate::state::pane_state::PaneSession;
use crate::state::workspaces::WorkspaceStore;
use crate::ui::explorer::ExplorerPanel;
use crate::ui::inspector::InspectorPanel;
use crate::ui::sidebar::NavPanel;
use crate::ui::titlebar::PikuTitleBar;

const DOCK_ID: &str = "piku-dock";
const DOCK_VERSION: usize = 1;

/// Relative path of the active workspace's layout file.
fn active_layout_file(cx: &App) -> String {
    WorkspaceStore::layout_file(PikuState::global(cx).workspaces.read(cx).active_id())
}

pub struct Workspace {
    title_bar: Entity<PikuTitleBar>,
    dock_area: Entity<DockArea>,
    last_layout: Option<DockAreaState>,
    _save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock_area =
            cx.new(|cx| DockArea::new(DOCK_ID, Some(DOCK_VERSION), window, cx));
        let weak_dock = dock_area.downgrade();

        if Self::load_layout(&dock_area, window, cx).is_err() {
            Self::reset_default_layout(&weak_dock, window, cx);
        }

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &dock_area,
            window,
            |this: &mut Self, dock_area, event: &DockEvent, window, cx| {
                if matches!(event, DockEvent::LayoutChanged) {
                    this.save_layout_debounced(dock_area.clone(), window, cx);
                }
            },
        ));

        // Status bar re-renders with selection and job updates.
        let state = PikuState::global(cx);
        let selection = state.selection.clone();
        let jobs = state.jobs.clone();
        subscriptions.push(cx.observe(&selection, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&jobs, |_, _, cx| cx.notify()));

        cx.on_app_quit({
            let dock_area = dock_area.clone();
            move |_, cx| {
                let state = dock_area.read(cx).dump(cx);
                let file = active_layout_file(cx);
                cx.background_executor().spawn(async move {
                    let _ = crate::state::persistence::save_json(&file, &state);
                })
            }
        })
        .detach();

        let title_bar = cx.new(|cx| PikuTitleBar::new(dock_area.downgrade(), window, cx));

        Self {
            title_bar,
            dock_area,
            last_layout: None,
            _save_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn dock_area(&self) -> &Entity<DockArea> {
        &self.dock_area
    }

    // -- Layout persistence ------------------------------------------------

    fn load_layout(
        dock_area: &Entity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<()> {
        let state: DockAreaState = crate::state::persistence::load_json(&active_layout_file(cx))?;
        if state.version != Some(DOCK_VERSION) {
            anyhow::bail!("layout version changed");
        }
        dock_area.update(cx, |dock_area, cx| {
            dock_area.load(state, window, cx)?;
            dock_area.set_dock_collapsible(
                Edges {
                    left: true,
                    right: true,
                    ..Default::default()
                },
                window,
                cx,
            );
            Ok(())
        })
    }

    fn reset_default_layout(
        dock_area: &WeakEntity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dock) = dock_area.upgrade() else {
            return;
        };

        let home = crate::services::fs_service::home_dir();
        let explorer = cx.new(|cx| ExplorerPanel::from_session(PaneSession::at(home), window, cx));
        let center = DockItem::tabs(vec![Arc::new(explorer)], dock_area, window, cx);

        let nav = cx.new(|cx| NavPanel::new(window, cx));
        let left = DockItem::tab(nav, dock_area, window, cx);

        let inspector = cx.new(|cx| InspectorPanel::new(window, cx));
        let right = DockItem::tab(inspector, dock_area, window, cx);

        dock.update(cx, |dock_area, cx| {
            dock_area.set_version(DOCK_VERSION, window, cx);
            dock_area.set_center(center, window, cx);
            dock_area.set_left_dock(left, Some(px(248.)), true, window, cx);
            dock_area.set_right_dock(right, Some(px(300.)), true, window, cx);
            dock_area.set_dock_collapsible(
                Edges {
                    left: true,
                    right: true,
                    ..Default::default()
                },
                window,
                cx,
            );
        });
    }

    fn save_layout_debounced(
        &mut self,
        dock_area: Entity<DockArea>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._save_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(2))
                .await;
            let _ = this.update(cx, move |this: &mut Workspace, cx| {
                let state = dock_area.read(cx).dump(cx);
                if this.last_layout.as_ref() == Some(&state) {
                    return;
                }
                this.last_layout = Some(state.clone());
                // Resolve the target file on the UI thread: the write must go
                // to the workspace that owns this layout.
                let file = active_layout_file(cx);
                cx.background_executor()
                    .spawn(async move {
                        if let Err(error) = crate::state::persistence::save_json(&file, &state) {
                            tracing::warn!("failed to save layout: {error:#}");
                        }
                    })
                    .detach();
            });
        }));
    }

    // -- Pane management ---------------------------------------------------

    fn active_session(&self, cx: &Context<Self>) -> PaneSession {
        PikuState::global(cx)
            .active_explorer()
            .and_then(|weak| weak.upgrade())
            .map(|panel| PaneSession::at(panel.read(cx).cwd().clone()))
            .unwrap_or_else(|| PaneSession::at(crate::services::fs_service::home_dir()))
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.active_session(cx);
        let panel = cx.new(|cx| ExplorerPanel::from_session(session, window, cx));
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.add_panel(Arc::new(panel), DockPlacement::Center, None, window, cx);
        });
    }

    fn split(&mut self, placement: Placement, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.active_session(cx);
        let tab_panel = PikuState::global(cx)
            .active_explorer()
            .and_then(|weak| weak.upgrade())
            .and_then(|panel| panel.read(cx).tab_panel())
            .and_then(|weak| weak.upgrade());

        let panel = cx.new(|cx| ExplorerPanel::from_session(session, window, cx));
        match tab_panel {
            Some(tab_panel) => {
                tab_panel.update(cx, |tab_panel, cx| {
                    tab_panel.add_panel_at(Arc::new(panel), placement, None, window, cx);
                });
            }
            None => {
                self.dock_area.update(cx, |dock_area, cx| {
                    dock_area.add_panel(Arc::new(panel), DockPlacement::Center, None, window, cx);
                });
            }
        }
    }

    // -- Workspaces --------------------------------------------------------

    /// Persist the active workspace's dock layout right now (bypassing the
    /// debounce). Used before switching, duplicating, or deleting.
    fn save_active_layout_now(&mut self, cx: &mut Context<Self>) {
        self._save_task = None;
        let state = self.dock_area.read(cx).dump(cx);
        self.last_layout = Some(state.clone());
        let file = active_layout_file(cx);
        if let Err(error) = crate::state::persistence::save_json(&file, &state) {
            tracing::warn!("failed to save layout: {error:#}");
        }
        PikuState::global(cx).nav.read(cx).save();
    }

    fn switch_workspace(&mut self, target_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let state = PikuState::global(cx);
        let store = state.workspaces.clone();
        let nav = state.nav.clone();
        let current_id = store.read(cx).active_id().to_string();
        if target_id == current_id {
            return;
        }
        if !store.read(cx).list().iter().any(|w| w.id == target_id) {
            return;
        }

        // 1. Save the outgoing workspace synchronously.
        self.save_active_layout_now(cx);

        // 2. Flip the index.
        store.update(cx, |store, cx| {
            store.set_active(target_id);
            cx.notify();
        });

        // 3. Swap the NavModel contents in place so every existing observer
        //    (nav panel, context menus) keeps working against the same entity.
        //    Must happen before the dock reload: restored panels read it.
        let fresh = NavModel::load_for(target_id);
        nav.update(cx, |nav, cx| {
            *nav = fresh;
            cx.notify();
        });

        // 4. Rebuild the dock from the target's layout — same path as
        //    startup. In-place `DockArea::load` keeps the title bar's weak
        //    ref and our DockEvent subscription alive; dropped panels' weak
        //    refs simply stop upgrading.
        self.last_layout = None;
        if Self::load_layout(&self.dock_area, window, cx).is_err() {
            Self::reset_default_layout(&self.dock_area.downgrade(), window, cx);
        }
        cx.notify();
    }

    fn on_create_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity();
        crate::ui::workspace_dialogs::prompt_workspace_name(
            "New workspace",
            "Create",
            String::new(),
            move |name, window, cx| {
                let store = PikuState::global(cx).workspaces.clone();
                let created = store.update(cx, |store, _| store.create(&name));
                match created {
                    Ok(id) => {
                        this.update(cx, |this, cx| this.switch_workspace(&id, window, cx));
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            },
            window,
            cx,
        );
    }

    fn on_rename_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = PikuState::global(cx).workspaces.clone();
        let (id, current_name) = {
            let active = store.read(cx).active();
            (active.id.clone(), active.name.clone())
        };
        crate::ui::workspace_dialogs::prompt_workspace_name(
            "Rename workspace",
            "Rename",
            current_name,
            move |name, _, cx| {
                let store = PikuState::global(cx).workspaces.clone();
                store.update(cx, |store, cx| {
                    let result = store.rename(&id, &name);
                    cx.notify();
                    result
                })
            },
            window,
            cx,
        );
    }

    fn on_duplicate_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Capture the live layout so the duplicate matches what is on screen.
        self.save_active_layout_now(cx);
        let store = PikuState::global(cx).workspaces.clone();
        let active_id = store.read(cx).active_id().to_string();
        let duplicated = store.update(cx, |store, cx| {
            let result = store.duplicate(&active_id);
            cx.notify();
            result
        });
        match duplicated {
            Ok(new_id) => self.switch_workspace(&new_id, window, cx),
            Err(error) => window.push_notification(Notification::error(error), cx),
        }
    }

    fn on_delete_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = PikuState::global(cx).workspaces.clone();
        let (id, name, fallback) = {
            let store = store.read(cx);
            let active = store.active();
            (
                active.id.clone(),
                active.name.clone(),
                store.most_recent_other(&active.id).map(|w| w.id.clone()),
            )
        };
        let Some(fallback) = fallback else {
            window.push_notification(Notification::info("Cannot delete the last workspace"), cx);
            return;
        };

        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let this = this.clone();
            let id = id.clone();
            let fallback = fallback.clone();
            alert
                .confirm()
                .title(format!("Delete workspace “{name}”?"))
                .description(
                    "Its saved layout and navigation lists will be removed. \
                     Files on disk are not affected.",
                )
                .on_ok(move |_, window, cx| {
                    let this = this.clone();
                    let id = id.clone();
                    let fallback = fallback.clone();
                    let _ = this.update(cx, |this, cx| {
                        // Deleting the active workspace: switch away first.
                        this.switch_workspace(&fallback, window, cx);
                        let store = PikuState::global(cx).workspaces.clone();
                        let result = store.update(cx, |store, cx| {
                            let result = store.delete(&id);
                            cx.notify();
                            result
                        });
                        if let Err(error) = result {
                            window.push_notification(Notification::error(error), cx);
                        }
                    });
                    true
                })
        });
    }

    fn on_show_about(&mut self, _: &ShowAbout, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |dialog, _, _| {
            dialog
                .title("About")
                .w(px(360.))
                .overlay_closable(true)
                .content(|content, _, cx| {
                    content.child(crate::ui::titlebar::about_content(cx))
                })
        });
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);

        div()
            .id("piku-workspace")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(|this, _: &SplitRight, window, cx| {
                this.split(Placement::Right, window, cx);
            }))
            .on_action(cx.listener(|this, _: &SplitDown, window, cx| {
                this.split(Placement::Bottom, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleLeftDock, window, cx| {
                this.dock_area.update(cx, |dock_area, cx| {
                    dock_area.toggle_dock(DockPlacement::Left, window, cx);
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleRightDock, window, cx| {
                this.dock_area.update(cx, |dock_area, cx| {
                    dock_area.toggle_dock(DockPlacement::Right, window, cx);
                });
            }))
            .on_action(cx.listener(Self::on_show_about))
            .on_action(cx.listener(|this, action: &SwitchWorkspace, window, cx| {
                let id = action.0.clone();
                this.switch_workspace(&id, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CreateWorkspace, window, cx| {
                this.on_create_workspace(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RenameWorkspace, window, cx| {
                this.on_rename_workspace(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DuplicateWorkspace, window, cx| {
                this.on_duplicate_workspace(window, cx);
            }))
            .on_action(cx.listener(|this, _: &DeleteWorkspace, window, cx| {
                this.on_delete_workspace(window, cx);
            }))
            .on_action(cx.listener(|_, action: &ToggleFavoritePath, _, cx| {
                let path = action.0.clone();
                PikuState::global(cx)
                    .nav
                    .clone()
                    .update(cx, |nav, cx| nav.toggle_favorite(path, cx));
            }))
            .on_action(cx.listener(|_, action: &TogglePinnedPath, _, cx| {
                let path = action.0.clone();
                PikuState::global(cx)
                    .nav
                    .clone()
                    .update(cx, |nav, cx| nav.toggle_pinned(path, cx));
            }))
            .on_action(cx.listener(|_, action: &RemoveRecentPath, _, cx| {
                let path = action.0.clone();
                PikuState::global(cx)
                    .nav
                    .clone()
                    .update(cx, |nav, cx| nav.remove_recent(&path, cx));
            }))
            .child(self.title_bar.clone())
            .child(div().flex_1().min_h_0().child(self.dock_area.clone()))
            .child(crate::ui::statusbar::render_status_bar(self, cx))
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
