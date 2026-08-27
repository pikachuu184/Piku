//! Left-dock navigation panel: places, favorites, pinned, recents, drives.

use std::path::{Path, PathBuf};

use gpui::{
    App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled,
    Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, WindowExt as _,
    dock::{Panel, PanelEvent},
    h_flex,
    input::{InputEvent, InputState},
    menu::ContextMenuExt as _,
    v_flex,
};

use crate::app::actions::{RemoveRecentPath, ToggleFavoritePath, TogglePinnedPath};
use crate::app::assets::PikuIcon;
use crate::backend::dispatch::BackendExt as _;
use crate::security::text::{sanitize_label, sanitize_path};
use crate::services::fs_service::{DriveInfo, Place};
use crate::state::PikuState;
use crate::ui::components::skeleton_rows;
use crate::ui::explorer::navigate_active;
use crate::ui::sidebar::section::section;

/// Inline workspace name editor shown in the switcher header — creation and
/// rename happen in place, no modal (same pattern as the explorer's inline
/// file editing: Enter commits, Escape and blur cancel).
pub(super) struct WsEdit {
    /// True = creating a new workspace; false = renaming the active one.
    create: bool,
    /// The name before editing, so an unchanged rename is a silent no-op.
    original: String,
    pub(super) input: gpui::Entity<InputState>,
    _subs: Vec<Subscription>,
}

pub struct NavPanel {
    focus_handle: FocusHandle,
    places: Vec<Place>,
    drives: Vec<DriveInfo>,
    drives_loaded: bool,
    /// Places used to be enumerated synchronously in `new`, so the section was
    /// populated on frame one. Now that it is dispatched, this drives a
    /// skeleton that reserves the same height — without it the section body
    /// has zero height for the first frames and everything below it jumps.
    places_loaded: bool,
    pub(super) ws_edit: Option<WsEdit>,
    _subscriptions: Vec<Subscription>,
}

impl NavPanel {
    pub const PANEL_NAME: &'static str = "PikuNav";

    /// Placeholder rows shown while `known_places` is in flight. Matches the
    /// usual number of existing well-known directories closely enough that the
    /// section does not visibly resize when the real rows arrive.
    const PLACES_SKELETON_ROWS: usize = 5;

    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (nav, workspaces, drive_stats) = {
            let state = PikuState::global(cx);
            (
                state.nav.clone(),
                state.workspaces.clone(),
                state.drive_stats.clone(),
            )
        };
        let subscription = cx.observe(&nav, |_, _, cx| cx.notify());
        // Re-render the switcher header on workspace switch/rename.
        let workspaces_sub = cx.observe(&workspaces, |_, _, cx| cx.notify());
        // Repaint drive bars as scan results stream in.
        let drive_stats_sub = cx.observe(&drive_stats, |_, _, cx| cx.notify());

        // Drive enumeration refreshes the whole mount table — dispatched to
        // the backend, which also caches it so repeated sidebar rebuilds are
        // free.
        cx.backend_task(
            |backend| backend.drive().drives(),
            |this: &mut NavPanel, drives, cx| {
                // Mark loaded either way: a failure must retire the skeleton,
                // or the sidebar shows placeholder rows forever.
                this.drives_loaded = true;
                let Ok(Ok(drives)) = drives else {
                    return;
                };
                this.drives = drives;
                // Drives are known — refresh any stale per-category usage
                // stats in the background (silent, cached, sequential).
                let mounts: Vec<PathBuf> = this.drives.iter().map(|d| d.mount.clone()).collect();
                // Bind before `update`: the global borrow must end before `cx`
                // can be taken mutably.
                let drive_stats = PikuState::global(cx).drive_stats.clone();
                drive_stats.update(cx, |store, cx| {
                    store.ensure_scans(mounts, cx);
                });
            },
        );

        // Register so the shell can start inline workspace edits from the
        // global Create/Rename actions.
        PikuState::global(cx).set_nav_panel(cx.entity().downgrade());

        // Git awareness for Places: discovery is negative-cached and bounded
        // by the handful of known places, so this is a one-shot cheap probe.
        // Rows re-render (branch badge) whenever repository state changes.
        let git = PikuState::global(cx).git.clone();
        let git_sub = cx.observe(&git, |_, _, cx| cx.notify());

        // `known_places` probes seven directories for existence. That used to
        // run right here, on the render thread, during every layout restore.
        cx.backend_task(
            |backend| backend.drive().places(),
            |this: &mut NavPanel, places, cx| {
                this.places_loaded = true;
                let Ok(Ok(places)) = places else {
                    return;
                };
                let git = PikuState::global(cx).git.clone();
                git.update(cx, |git, cx| {
                    for place in &places {
                        git.note_dir(place.path.clone(), cx);
                    }
                });
                this.places = places;
            },
        );

        Self {
            focus_handle: cx.focus_handle(),
            places: Vec::new(),
            places_loaded: false,
            drives: Vec::new(),
            drives_loaded: false,
            ws_edit: None,
            _subscriptions: vec![subscription, workspaces_sub, drive_stats_sub, git_sub],
        }
    }

    // -- Inline workspace editing -------------------------------------------

    /// Open the inline editor in the switcher header. `create` picks between
    /// creating a new workspace and renaming the active one.
    pub fn start_workspace_edit(
        &mut self,
        create: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.ws_edit.is_some() {
            return;
        }
        let original = if create {
            String::new()
        } else {
            PikuState::global(cx)
                .workspaces
                .read(cx)
                .active()
                .name
                .clone()
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(original.clone())
                .placeholder("Workspace name")
        });
        let mut subs = Vec::new();
        subs.push(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_workspace_edit(window, cx),
                InputEvent::Blur => this.cancel_workspace_edit(cx),
                _ => {}
            },
        ));
        input.update(cx, |input, cx| input.focus(window, cx));
        self.ws_edit = Some(WsEdit {
            create,
            original,
            input,
            _subs: subs,
        });
        cx.notify();
    }

    pub(super) fn cancel_workspace_edit(&mut self, cx: &mut Context<Self>) {
        if self.ws_edit.take().is_some() {
            cx.notify();
        }
    }

    fn commit_workspace_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.ws_edit.take() else {
            return;
        };
        let name = edit.input.read(cx).value().trim().to_string();
        if name.is_empty() || (!edit.create && name == edit.original) {
            cx.notify();
            return;
        }

        let store = PikuState::global(cx).workspaces.clone();
        let result = if edit.create {
            store.update(cx, |store, cx| {
                let result = store.create(&name);
                cx.notify();
                result.map(Some)
            })
        } else {
            let id = store.read(cx).active_id().to_string();
            store.update(cx, |store, cx| {
                let result = store.rename(&id, &name);
                cx.notify();
                result.map(|_| None)
            })
        };

        match result {
            // A freshly created workspace becomes active — the shell owns the
            // switch logic, so route through the global action.
            Ok(Some(new_id)) => {
                window.dispatch_action(Box::new(crate::app::actions::SwitchWorkspace(new_id)), cx);
            }
            Ok(None) => {}
            Err(error) => {
                // Keep the editor open so the user can fix the name.
                window.push_notification(crate::ui::toast::error(error), cx);
                self.ws_edit = Some(edit);
                return;
            }
        }
        cx.notify();
    }

    /// Escape closes the workspace editor (wired on the editor wrapper).
    pub(super) fn on_ws_editor_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.cancel_workspace_edit(cx);
            window.focus(&self.focus_handle, cx);
        }
    }

    /// Collapse state lives on the workspace's NavModel so each workspace
    /// restores its own sidebar shape across restarts.
    fn is_open(&self, id: &'static str, cx: &Context<Self>) -> bool {
        !PikuState::global(cx).nav.read(cx).is_section_collapsed(id)
    }

    fn toggle(&mut self, id: &'static str, cx: &mut Context<Self>) {
        PikuState::global(cx).nav.clone().update(cx, |nav, cx| {
            nav.toggle_section(id, cx);
        });
        cx.notify();
    }

    fn nav_row(
        &self,
        id: SharedString,
        icon: Icon,
        label: SharedString,
        path: PathBuf,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let click_path = path.clone();
        let menu_path = path.clone();
        let tooltip_text = sanitize_path(&path);
        h_flex()
            .id(id)
            .items_center()
            .gap_2()
            .mx_1()
            .px_2()
            .py_1()
            .rounded(cx.theme().radius)
            .cursor_pointer()
            .text_sm()
            .text_color(cx.theme().sidebar_foreground)
            .hover(|style| {
                style
                    .bg(cx.theme().sidebar_accent)
                    .text_color(cx.theme().sidebar_accent_foreground)
            })
            .on_click(move |_, window, cx| {
                navigate_active(click_path.clone(), window, cx);
            })
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(tooltip_text.clone()).build(window, cx)
            })
            .child(icon.size(px(16.)).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().min_w_0().truncate().child(label))
            .children({
                // Branch badge when this place is itself a repository (the
                // name is pre-sanitized and capped by the git store).
                let git = PikuState::global(cx).git.read(cx);
                git.root_for(&path)
                    .and_then(|root| git.snapshot(root))
                    .and_then(|snap| snap.branch.clone())
                    .map(|branch| {
                        h_flex()
                            .gap_0p5()
                            .items_center()
                            .flex_none()
                            .max_w(px(90.))
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(PikuIcon::GitBranch).size(px(11.)))
                            .child(div().truncate().child(branch))
                    })
            })
            .context_menu(move |menu, _, cx| {
                let nav = PikuState::global(cx).nav.read(cx);
                let is_favorite = nav.is_favorite(&menu_path);
                let is_pinned = nav.is_pinned(&menu_path);
                let is_recent = nav.recents.iter().any(|p| p == &menu_path);
                let menu = menu
                    .menu(
                        if is_favorite {
                            "Remove from favorites"
                        } else {
                            "Add to favorites"
                        },
                        Box::new(ToggleFavoritePath(menu_path.clone())),
                    )
                    .menu(
                        if is_pinned { "Unpin" } else { "Pin" },
                        Box::new(TogglePinnedPath(menu_path.clone())),
                    );
                if is_recent {
                    menu.separator().menu(
                        "Remove from recents",
                        Box::new(RemoveRecentPath(menu_path.clone())),
                    )
                } else {
                    menu
                }
            })
    }

    fn rows_for_paths<'a>(
        &self,
        prefix: &'static str,
        icon_for: impl Fn(&Path) -> Icon + 'a,
        paths: impl Iterator<Item = &'a PathBuf> + 'a,
        cx: &Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        paths
            .enumerate()
            .map(|(ix, path)| {
                let label: SharedString = path
                    .file_name()
                    .map(|n| sanitize_label(&n.to_string_lossy()))
                    .unwrap_or_else(|| sanitize_path(path))
                    .into();
                gpui::IntoElement::into_any_element(self.nav_row(
                    SharedString::from(format!("{prefix}-{ix}")),
                    icon_for(path),
                    label,
                    path.clone(),
                    cx,
                ))
            })
            .collect()
    }
}

impl Render for NavPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("NavPanel");
        let state = PikuState::global(cx);
        let nav = state.nav.read(cx);
        let favorites: Vec<PathBuf> = nav.favorites.clone();
        let pinned: Vec<PathBuf> = nav.pinned.clone();
        let recents: Vec<PathBuf> = nav.recents.iter().cloned().collect();

        let place_icon = |place: &Place| -> Icon {
            use crate::services::fs_service::PlaceKind::*;
            match place.kind {
                Home => Icon::new(PikuIcon::House),
                Desktop => Icon::new(PikuIcon::Monitor),
                Documents => Icon::new(PikuIcon::FileText),
                Downloads => Icon::new(IconName::Inbox),
                Pictures => Icon::new(PikuIcon::Image),
                Music => Icon::new(PikuIcon::Music),
                Videos => Icon::new(PikuIcon::Film),
            }
        };

        let places_content = if !self.places_loaded {
            // Same row height and count as a populated Places list, so the
            // section does not change height when the results land.
            v_flex()
                .gap_1()
                .child(div().mx_1().px_2().py_1p5().child(skeleton_rows(
                    Self::PLACES_SKELETON_ROWS,
                    24.,
                    cx,
                )))
        } else if self.places.is_empty() {
            v_flex().child(empty_hint("No known locations", cx))
        } else {
            v_flex().gap_0p5().children(
                self.places
                    .iter()
                    .enumerate()
                    .map(|(ix, place)| {
                        self.nav_row(
                            SharedString::from(format!("place-{ix}")),
                            place_icon(place),
                            SharedString::from(place.name),
                            place.path.clone(),
                            cx,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        };

        let favorites_content = if favorites.is_empty() {
            v_flex().child(empty_hint("No favorites yet", cx))
        } else {
            v_flex().gap_0p5().children(self.rows_for_paths(
                "fav",
                |_| Icon::new(IconName::Star),
                favorites.iter(),
                cx,
            ))
        };

        let pinned_content = if pinned.is_empty() {
            v_flex().child(empty_hint("Nothing pinned", cx))
        } else {
            v_flex().gap_0p5().children(self.rows_for_paths(
                "pin",
                |_| Icon::new(PikuIcon::Pin),
                pinned.iter(),
                cx,
            ))
        };

        let recents_content = if recents.is_empty() {
            v_flex().child(empty_hint("No recent locations", cx))
        } else {
            v_flex().gap_0p5().children(self.rows_for_paths(
                "recent",
                |_| Icon::new(PikuIcon::Clock),
                recents.iter(),
                cx,
            ))
        };

        let drives_content = if !self.drives_loaded {
            // Drive enumeration is still running on the background executor.
            v_flex().gap_1().child(
                div()
                    .mx_1()
                    .px_2()
                    .py_1p5()
                    .child(skeleton_rows(3, 24., cx)),
            )
        } else {
            // Ring tiles flow two per row and wrap with the drive count.
            v_flex().child(
                h_flex().flex_wrap().mx_1().children(
                    self.drives
                        .iter()
                        .enumerate()
                        .map(|(ix, drive)| {
                            let path = drive.mount.clone();
                            let summary = super::drive_list::drive_summary(drive, cx);
                            div()
                                .id(SharedString::from(format!("drive-{ix}")))
                                .w_1_2()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .hover(|style| style.bg(cx.theme().sidebar_accent))
                                .on_click(move |_, window, cx| {
                                    navigate_active(path.clone(), window, cx);
                                })
                                .tooltip(move |window, cx| {
                                    gpui_component::tooltip::Tooltip::new(summary.clone())
                                        .build(window, cx)
                                })
                                .child(super::drive_list::drive_tile(drive, cx))
                        })
                        .collect::<Vec<_>>(),
                ),
            )
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(super::workspace_switcher::workspace_switcher(self, cx))
            .child(
                div()
                    .id("nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .py_2()
                            .gap_2()
                            .child(section(
                                "sec-places",
                                "Places",
                                self.is_open("places", cx),
                                cx.listener(|this, _, _, cx| this.toggle("places", cx)),
                                places_content,
                                cx,
                            ))
                            .child(section(
                                "sec-favorites",
                                "Favorites",
                                self.is_open("favorites", cx),
                                cx.listener(|this, _, _, cx| this.toggle("favorites", cx)),
                                favorites_content,
                                cx,
                            ))
                            .child(section(
                                "sec-pinned",
                                "Pinned",
                                self.is_open("pinned", cx),
                                cx.listener(|this, _, _, cx| this.toggle("pinned", cx)),
                                pinned_content,
                                cx,
                            ))
                            .child(section(
                                "sec-recents",
                                "Recents",
                                self.is_open("recents", cx),
                                cx.listener(|this, _, _, cx| this.toggle("recents", cx)),
                                recents_content,
                                cx,
                            ))
                            .child(section(
                                "sec-drives",
                                "Drives",
                                self.is_open("drives", cx),
                                cx.listener(|this, _, _, cx| this.toggle("drives", cx)),
                                drives_content,
                                cx,
                            )),
                    ),
            )
    }
}

fn empty_hint(text: &'static str, cx: &App) -> gpui::Div {
    div()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

impl Focusable for NavPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for NavPanel {}

impl Panel for NavPanel {
    fn panel_name(&self) -> &'static str {
        Self::PANEL_NAME
    }

    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        "Navigation"
    }

    fn closable(&self, _: &App) -> bool {
        false
    }

    fn zoomable(&self, _: &App) -> Option<gpui_component::dock::PanelControl> {
        None
    }
}
