//! Workspace switcher header at the top of the navigation panel: shows the
//! active workspace and opens a menu to switch, create, rename, duplicate,
//! or delete workspaces.

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement, SharedString, Styled, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::DropdownMenu as _,
};

use crate::app::actions::{
    CreateWorkspace, DeleteWorkspace, DuplicateWorkspace, RenameWorkspace, SwitchWorkspace,
};
use crate::app::assets::PikuIcon;
use crate::state::PikuState;
use crate::ui::sidebar::NavPanel;

pub(super) fn workspace_switcher(
    panel: &NavPanel,
    cx: &mut Context<NavPanel>,
) -> impl IntoElement {
    let store = PikuState::global(cx).workspaces.read(cx);
    let active_name: SharedString = store.active().name.clone().into();
    // Inline create/rename: the name label swaps for a small input — same
    // row, same metrics, no modal.
    let editor = panel.ws_edit.as_ref().map(|edit| edit.input.clone());

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
        .child(match editor {
            Some(input) => div()
                .flex_1()
                .min_w_0()
                .rounded(cx.theme().radius)
                .on_key_down(cx.listener(NavPanel::on_ws_editor_key_down))
                .child(Input::new(&input).small())
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(cx.theme().sidebar_foreground)
                .child(active_name)
                .into_any_element(),
        })
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
