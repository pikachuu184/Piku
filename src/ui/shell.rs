//! The workspace shell: title bar, three-region dock area, status bar.
//! Layout is persisted as a logical dock graph and restored at startup.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Edges, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Placement, Root, WindowExt as _,
    dock::{DockArea, DockAreaState, DockEvent, DockItem, DockPlacement, PanelInfo, PanelState},
};

use crate::app::actions::{
    CreateWorkspace, DeleteWorkspace, DuplicateTab, DuplicateWorkspace, NewTab, OpenMediaPanel,
    PinTab, PreviewActualSize, PreviewFit, PreviewFitWidth, PreviewNextPage, PreviewPrevPage,
    PreviewResetView, PreviewRotate, PreviewZoomIn, PreviewZoomOut, RemoveRecentPath,
    RenameWorkspace, RevealPreview, ShowAbout, SplitDown, SplitRight, SwitchWorkspace,
    ToggleFavoritePath, ToggleLeftDock, TogglePinnedPath, ToggleRightDock, ToggleTransferCenter,
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
    media_bar: Entity<crate::ui::media::MediaBar>,
    last_layout: Option<DockAreaState>,
    /// Branch panel anchored above the status bar (toggled from its git
    /// segment); dropped entirely when closed.
    branch_popover: Option<Entity<crate::ui::git::BranchPopover>>,
    /// Transfer panel anchored above the status bar's transfer segment, which
    /// is on the right. Same lifecycle as `branch_popover`: dropped when
    /// closed, so a panel that is not on screen is not observing the store.
    transfer_popover: Option<Entity<crate::ui::transfers::popover::TransferPopover>>,
    /// Whether we believe a transfer center dialog is on the dialog stack, so
    /// `ToggleTransferCenter` closes it instead of stacking a second identical
    /// one. Cleared by the dialog's own `on_close`, which fires for all three
    /// ways out (close button, Escape, clicking the overlay).
    transfer_center: bool,
    _save_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let dock_area = cx.new(|cx| DockArea::new(DOCK_ID, Some(DOCK_VERSION), window, cx));
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

        // Status bar re-renders with selection, job, and git updates.
        let state = PikuState::global(cx);
        let selection = state.selection.clone();
        let jobs = state.jobs.clone();
        let git = state.git.clone();
        subscriptions.push(cx.observe(&selection, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&jobs, |_, _, cx| cx.notify()));
        subscriptions.push(cx.observe(&git, |_, _, cx| cx.notify()));

        cx.on_app_quit({
            let dock_area = dock_area.clone();
            move |_, cx| {
                // Write synchronously: a detached background task races
                // process teardown and the save silently never lands.
                let state = dock_area.read(cx).dump(cx);
                let file = active_layout_file(cx);
                let _ = crate::state::persistence::save_json(&file, &state);
                std::future::ready(())
            }
        })
        .detach();

        let title_bar = cx.new(|cx| PikuTitleBar::new(dock_area.downgrade(), window, cx));
        let media_bar = cx.new(crate::ui::media::MediaBar::new);

        let mut this = Self {
            title_bar,
            dock_area: dock_area.clone(),
            media_bar,
            last_layout: None,
            branch_popover: None,
            transfer_popover: None,
            transfer_center: false,
            _save_task: None,
            _subscriptions: subscriptions,
        };
        // Schedule an initial save so the workspace's layout file exists even
        // if the user never rearranges anything (no LayoutChanged fires).
        this.save_layout_debounced(dock_area, window, cx);
        this
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
        let mut state: DockAreaState =
            crate::state::persistence::load_json(&active_layout_file(cx))?;
        if state.version != Some(DOCK_VERSION) {
            anyhow::bail!("layout version changed");
        }
        // The center root must always be a StackPanel: TabPanel::split_panel
        // silently no-ops when a TabPanel has no parent stack, which is what
        // broke splitting on layouts saved with a bare-TabPanel center.
        if state.center.panel_name != "StackPanel" {
            let child = std::mem::take(&mut state.center);
            state.center = PanelState {
                panel_name: "StackPanel".into(),
                children: vec![child],
                info: PanelInfo::stack(Vec::new(), gpui::Axis::Horizontal),
            };
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
        let tabs = DockItem::tabs(vec![Arc::new(explorer)], dock_area, window, cx);
        // Wrap in a StackPanel so both split directions work from the first
        // pane (see the invariant note in `load_layout`).
        let center = DockItem::split(gpui::Axis::Horizontal, vec![tabs], dock_area, window, cx);

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
            cx.background_executor().timer(Duration::from_secs(2)).await;
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
            .map(|panel| panel.read(cx).new_tab_session())
            .unwrap_or_else(|| PaneSession::at(crate::services::fs_service::home_dir()))
    }

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        let session = self.active_session(cx);
        let panel = cx.new(|cx| ExplorerPanel::from_session(session, window, cx));
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.add_panel(Arc::new(panel), DockPlacement::Center, None, window, cx);
        });
    }

    /// Run `f` on the inspector panel, if the current layout has one.
    ///
    /// The preview shortcuts are handled here on the workspace root rather than
    /// on the panel because gpui resolves actions along the focus path, and while
    /// browsing it is the file list — a *sibling* of the inspector — that holds
    /// focus. An `on_action` on the panel could never fire for them. The
    /// workspace is an ancestor of both, so it can.
    ///
    /// The handle is weak and resolved per action: the inspector is built either
    /// by `reset_default_layout` or by the `register_panel` deserializer, and a
    /// user can drag it out of the right dock, so nothing here may assume it
    /// exists.
    fn with_inspector(
        cx: &mut App,
        f: impl FnOnce(&mut InspectorPanel, &mut Context<InspectorPanel>),
    ) {
        let Some(panel) = PikuState::global(cx)
            .inspector()
            .and_then(|panel| panel.upgrade())
        else {
            return;
        };
        panel.update(cx, f);
    }

    /// Open (or add) a dockable media panel for `path` as a center tab. From
    /// there the user can drag/split/tab it like any other panel.
    fn open_media_panel(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let panel = cx.new(|cx| crate::ui::media::MediaPanel::for_path(path, window, cx));
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.add_panel(Arc::new(panel), DockPlacement::Center, None, window, cx);
        });
    }

    /// Duplicate the active tab into a new tab carrying its full session
    /// (folder, history, sort/view/zoom, filter, search, selection) but an
    /// independent identity. Falls back to a plain new tab if none is active.
    fn on_duplicate_tab(&mut self, _: &DuplicateTab, window: &mut Window, cx: &mut Context<Self>) {
        let session = PikuState::global(cx)
            .active_explorer()
            .and_then(|weak| weak.upgrade())
            .map(|panel| panel.read(cx).duplicate_session())
            .unwrap_or_else(|| self.active_session(cx));
        let panel = cx.new(|cx| ExplorerPanel::from_session(session, window, cx));
        self.dock_area.update(cx, |dock_area, cx| {
            dock_area.add_panel(Arc::new(panel), DockPlacement::Center, None, window, cx);
        });
    }

    /// Toggle the active tab's pinned state (from the tab dropdown menu).
    fn on_pin_tab(&mut self, _: &PinTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = PikuState::global(cx)
            .active_explorer()
            .and_then(|weak| weak.upgrade())
        {
            panel.update(cx, |panel, cx| panel.toggle_pin(cx));
        }
    }

    /// Toggle the branch panel for the repository containing the active
    /// pane's directory (invoked from the status bar's git segment).
    pub fn toggle_branch_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.branch_popover.take().is_some() {
            cx.notify();
            return;
        }
        let state = PikuState::global(cx);
        let root = state
            .selection
            .read(cx)
            .dir
            .as_deref()
            .and_then(|dir| state.git.read(cx).root_for(dir).map(|r| r.to_path_buf()));
        if let Some(root) = root {
            self.branch_popover =
                Some(cx.new(|cx| crate::ui::git::BranchPopover::new(root, window, cx)));
        }
        cx.notify();
    }

    /// Toggle the compact transfer panel above the status bar's transfer
    /// segment (invoked from that segment).
    ///
    /// No `Window`, unlike its git counterpart: the panel holds no input, so
    /// there is nothing to focus when it appears.
    pub fn toggle_transfer_popover(&mut self, cx: &mut Context<Self>) {
        self.transfer_popover = match self.transfer_popover.take() {
            Some(_) => None,
            None => Some(cx.new(crate::ui::transfers::popover::TransferPopover::new)),
        };
        cx.notify();
    }

    /// Open the transfer center, or close the one already open.
    ///
    /// The only path into the center. The popover's `Details` dispatches this
    /// action rather than opening the dialog itself, so `transfer_center` cannot
    /// disagree with what is on screen — and a second `ctrl-shift-t` closes the
    /// center instead of stacking an identical one on top of it.
    ///
    /// The closing half asks the dialog layer for its *topmost* dialog, which is
    /// all it offers — it does not say which one that is. So with a conflict
    /// surface or an error alert stacked over the center, this dismisses that
    /// instead. Nothing is lost when it happens: dismissing a conflict surface
    /// sends no decision and leaves the job waiting, which is the same as
    /// pressing Escape.
    fn on_toggle_transfer_center(
        &mut self,
        _: &ToggleTransferCenter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.transfer_center {
            self.transfer_center = false;
            window.close_dialog(cx);
            return;
        }
        // The center supersedes the popover. Left open, it would sit behind the
        // dialog's overlay as a panel the user can see and cannot click.
        self.transfer_popover = None;
        self.transfer_center = true;
        let this = cx.entity();
        crate::ui::transfers::center::open(
            move |_, cx| {
                this.update(cx, |this, _| this.transfer_center = false);
            },
            window,
            cx,
        );
        cx.notify();
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

    /// Create/rename happen inline in the workspace switcher header — no
    /// modal. The nav panel owns the editor; the shell just starts it.
    fn start_workspace_edit(&mut self, create: bool, window: &mut Window, cx: &mut Context<Self>) {
        // The switcher lives in the left dock: make sure it is visible.
        self.dock_area.update(cx, |dock_area, cx| {
            if !dock_area.is_dock_open(DockPlacement::Left, cx) {
                dock_area.toggle_dock(DockPlacement::Left, window, cx);
            }
        });
        if let Some(panel) = PikuState::global(cx)
            .nav_panel()
            .and_then(|weak| weak.upgrade())
        {
            panel.update(cx, |panel, cx| {
                panel.start_workspace_edit(create, window, cx);
            });
        }
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
            Err(error) => window.push_notification(crate::ui::toast::error(error), cx),
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
            window.push_notification(
                crate::ui::toast::info("Cannot delete the last workspace"),
                cx,
            );
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
                    this.update(cx, |this, cx| {
                        // Deleting the active workspace: switch away first.
                        this.switch_workspace(&fallback, window, cx);
                        let store = PikuState::global(cx).workspaces.clone();
                        let result = store.update(cx, |store, cx| {
                            let result = store.delete(&id);
                            cx.notify();
                            result
                        });
                        if let Err(error) = result {
                            window.push_notification(crate::ui::toast::error(error), cx);
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
                .content(|content, _, cx| content.child(crate::ui::titlebar::about_content(cx)))
        });
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("Workspace");
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
            .on_action(cx.listener(Self::on_duplicate_tab))
            .on_action(cx.listener(Self::on_pin_tab))
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
            // `space`, or double-clicking a previewable file, reveals (never
            // hides) the inspector dock so the preview is visible immediately —
            // and selects the Preview tab, which is the other half of the same
            // promise. Revealing the dock on the Details tab shows metadata.
            .on_action(cx.listener(|this, _: &RevealPreview, window, cx| {
                this.dock_area.update(cx, |dock_area, cx| {
                    if !dock_area.is_dock_open(DockPlacement::Right, cx) {
                        dock_area.toggle_dock(DockPlacement::Right, window, cx);
                    }
                });
                Self::with_inspector(cx, |panel, cx| panel.show_preview_tab(cx));
            }))
            // The preview transform and page navigation. Each of these resolves
            // the panel and calls one method on it, so the shortcut and the
            // toolbar button it mirrors run the same code — see the control
            // surface in `inspector/inspector_panel.rs`.
            .on_action(cx.listener(|_, _: &PreviewZoomIn, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_zoom_by(1.25, cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewZoomOut, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_zoom_by(0.8, cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewFit, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_fit(cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewFitWidth, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_fit_width(cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewActualSize, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_actual_size(cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewResetView, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_reset_view(cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewRotate, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_rotate(cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewNextPage, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_step_page(1, cx));
            }))
            .on_action(cx.listener(|_, _: &PreviewPrevPage, _, cx| {
                Self::with_inspector(cx, |panel, cx| panel.preview_step_page(-1, cx));
            }))
            .on_action(cx.listener(|this, action: &OpenMediaPanel, window, cx| {
                this.open_media_panel(action.0.clone(), window, cx);
            }))
            .on_action(cx.listener(Self::on_show_about))
            .on_action(cx.listener(Self::on_toggle_transfer_center))
            .on_action(cx.listener(|this, action: &SwitchWorkspace, window, cx| {
                let id = action.0.clone();
                this.switch_workspace(&id, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CreateWorkspace, window, cx| {
                this.start_workspace_edit(true, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RenameWorkspace, window, cx| {
                this.start_workspace_edit(false, window, cx);
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
            .child(self.media_bar.clone())
            // Branch panel floating just above the status bar's git segment.
            .when_some(self.branch_popover.clone(), |root, popover| {
                root.child(
                    div()
                        .id("branch-popover-overlay")
                        .absolute()
                        .bottom(px(34.))
                        .left(px(8.))
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.branch_popover = None;
                            cx.notify();
                        }))
                        .child(popover),
                )
            })
            // Transfer panel floating just above the status bar's transfer
            // segment — the same overlay block, anchored right because that
            // segment sits on the right.
            .when_some(self.transfer_popover.clone(), |root, popover| {
                root.child(
                    div()
                        .id("transfer-popover-overlay")
                        .absolute()
                        .bottom(px(34.))
                        .right(px(8.))
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.transfer_popover = None;
                            cx.notify();
                        }))
                        .child(popover),
                )
            })
            .child(crate::ui::statusbar::render_status_bar(self, cx))
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}
