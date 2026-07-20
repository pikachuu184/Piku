//! Left-dock navigation panel: places, favorites, pinned, recents, drives.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement as _, Styled, Subscription,
    Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName,
    dock::{Panel, PanelEvent},
    h_flex,
    menu::ContextMenuExt as _,
    v_flex,
};

use crate::app::actions::{RemoveRecentPath, ToggleFavoritePath, TogglePinnedPath};
use crate::app::assets::PikuIcon;
use crate::services::fs_service::{self, DriveInfo, Place};
use crate::state::PikuState;
use crate::ui::components::skeleton_rows;
use crate::ui::explorer::navigate_active;
use crate::ui::sidebar::drive_list::drive_details;
use crate::ui::sidebar::section::section;

pub struct NavPanel {
    focus_handle: FocusHandle,
    collapsed: HashSet<&'static str>,
    places: Vec<Place>,
    drives: Vec<DriveInfo>,
    drives_loaded: bool,
    _subscriptions: Vec<Subscription>,
}

impl NavPanel {
    pub const PANEL_NAME: &'static str = "PikuNav";

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (nav, workspaces) = {
            let state = PikuState::global(cx);
            (state.nav.clone(), state.workspaces.clone())
        };
        let subscription = cx.observe(&nav, |_, _, cx| cx.notify());
        // Re-render the switcher header on workspace switch/rename.
        let workspaces_sub = cx.observe(&workspaces, |_, _, cx| cx.notify());

        // Drive enumeration touches the disk subsystem — keep it off the
        // render thread.
        cx.spawn_in(window, async move |this, cx| {
            let drives = cx
                .background_executor()
                .spawn(async move { fs_service::list_drives() })
                .await;
            let _ = this.update(cx, |this: &mut NavPanel, cx| {
                this.drives = drives;
                this.drives_loaded = true;
                cx.notify();
            });
        })
        .detach();

        Self {
            focus_handle: cx.focus_handle(),
            collapsed: HashSet::new(),
            places: fs_service::known_places(),
            drives: Vec::new(),
            drives_loaded: false,
            _subscriptions: vec![subscription, workspaces_sub],
        }
    }

    fn is_open(&self, id: &'static str) -> bool {
        !self.collapsed.contains(id)
    }

    fn toggle(&mut self, id: &'static str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(id) {
            self.collapsed.insert(id);
        }
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
            .child(icon.size(px(16.)).text_color(cx.theme().muted_foreground))
            .child(div().flex_1().min_w_0().truncate().child(label))
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
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
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

        let places_content = v_flex().gap_0p5().children(
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
        );

        let favorites_content = if favorites.is_empty() {
            v_flex().child(empty_hint("No favorites yet", cx))
        } else {
            v_flex()
                .gap_0p5()
                .children(self.rows_for_paths("fav", |_| Icon::new(IconName::Star), favorites.iter(), cx))
        };

        let pinned_content = if pinned.is_empty() {
            v_flex().child(empty_hint("Nothing pinned", cx))
        } else {
            v_flex()
                .gap_0p5()
                .children(self.rows_for_paths("pin", |_| Icon::new(PikuIcon::Pin), pinned.iter(), cx))
        };

        let recents_content = if recents.is_empty() {
            v_flex().child(empty_hint("No recent locations", cx))
        } else {
            v_flex()
                .gap_0p5()
                .children(self.rows_for_paths(
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
            v_flex().gap_1().children(
            self.drives
                .iter()
                .enumerate()
                .map(|(ix, drive)| {
                    let path = drive.mount.clone();
                    h_flex()
                        .id(SharedString::from(format!("drive-{ix}")))
                        .items_start()
                        .gap_2()
                        .mx_1()
                        .px_2()
                        .py_1p5()
                        .rounded(cx.theme().radius)
                        .cursor_pointer()
                        .hover(|style| style.bg(cx.theme().sidebar_accent))
                        .on_click(move |_, window, cx| {
                            navigate_active(path.clone(), window, cx);
                        })
                        .child(
                            Icon::new(IconName::HardDrive)
                                .size(px(16.))
                                .text_color(cx.theme().muted_foreground),
                        )
                        .child(drive_details(drive, cx))
                })
                .collect::<Vec<_>>(),
            )
        };

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(super::workspace_switcher::workspace_switcher(cx))
            .child(
                div().id("nav-scroll").flex_1().min_h_0().overflow_y_scroll().child(
                    v_flex()
                        .py_2()
                        .gap_2()
                        .child(section(
                            "sec-places",
                            "Places",
                            self.is_open("places"),
                            cx.listener(|this, _, _, cx| this.toggle("places", cx)),
                            places_content,
                            cx,
                        ))
                        .child(section(
                            "sec-favorites",
                            "Favorites",
                            self.is_open("favorites"),
                            cx.listener(|this, _, _, cx| this.toggle("favorites", cx)),
                            favorites_content,
                            cx,
                        ))
                        .child(section(
                            "sec-pinned",
                            "Pinned",
                            self.is_open("pinned"),
                            cx.listener(|this, _, _, cx| this.toggle("pinned", cx)),
                            pinned_content,
                            cx,
                        ))
                        .child(section(
                            "sec-recents",
                            "Recents",
                            self.is_open("recents"),
                            cx.listener(|this, _, _, cx| this.toggle("recents", cx)),
                            recents_content,
                            cx,
                        ))
                        .child(section(
                            "sec-drives",
                            "Drives",
                            self.is_open("drives"),
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
