//! Workspace switcher header at the top of the navigation panel: shows the
//! active workspace and opens a menu to switch, create, rename, duplicate,
//! or delete workspaces.

use gpui::{Context, IntoElement, ParentElement, SharedString, Styled, div, px};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::DropdownMenu as _,
};

use crate::app::actions::{
    CreateWorkspace, DeleteWorkspace, DuplicateWorkspace, RenameWorkspace, SwitchWorkspace,
};
use crate::app::assets::PikuIcon;
use crate::state::PikuState;
use crate::ui::sidebar::NavPanel;

pub(super) fn workspace_switcher(cx: &Context<NavPanel>) -> impl IntoElement {
    let store = PikuState::global(cx).workspaces.read(cx);
    let active_name: SharedString = store.active().name.clone().into();

    h_flex()
        .w_full()
        .items_center()
        .gap_1()
        .px_2()
        .py_1p5()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            Icon::new(PikuIcon::Layers)
                .size(px(15.))
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().sidebar_foreground)
                .child(active_name),
        )
        .child(
            Button::new("workspace-menu")
                .icon(IconName::ChevronsUpDown)
                .xsmall()
                .ghost()
                .tooltip("Workspaces")
                .dropdown_menu(move |menu, _, cx| {
                    let store = PikuState::global(cx).workspaces.read(cx);
                    let active_id = store.active_id().to_string();
                    // Most recently opened first.
                    let mut workspaces: Vec<_> = store.list().to_vec();
                    workspaces.sort_by_key(|w| std::cmp::Reverse(w.last_opened));

                    let mut menu = menu;
                    for workspace in workspaces {
                        let checked = workspace.id == active_id;
                        menu = menu.menu_with_check(
                            SharedString::from(workspace.name.clone()),
                            checked,
                            Box::new(SwitchWorkspace(workspace.id.clone())),
                        );
                    }
                    menu.separator()
                        .menu("New workspace…", Box::new(CreateWorkspace))
                        .menu("Rename…", Box::new(RenameWorkspace))
                        .menu("Duplicate", Box::new(DuplicateWorkspace))
                        .menu("Delete…", Box::new(DeleteWorkspace))
                }),
        )
}
