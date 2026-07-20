//! Custom title bar: PIKU logo on the left, workspace controls on the right.

use gpui::{
    App, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement, Render,
    Styled, WeakEntity, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, TitleBar,
    button::{Button, ButtonVariants as _},
    dock::{DockArea, DockPlacement},
    h_flex,
    menu::DropdownMenu as _,
    v_flex,
};

use crate::app::actions::{NewTab, ShowAbout, SplitDown, SplitRight, ToggleHidden};
use crate::app::assets::PikuIcon;
use crate::app::logo::PikuLogo;
use crate::state::PikuState;

pub struct PikuTitleBar {
    dock_area: WeakEntity<DockArea>,
}

impl PikuTitleBar {
    pub fn new(dock_area: WeakEntity<DockArea>, _: &mut Window, _: &mut Context<Self>) -> Self {
        Self { dock_area }
    }

    fn toggle_dock(&self, placement: DockPlacement, window: &mut Window, cx: &mut App) {
        if let Some(dock_area) = self.dock_area.upgrade() {
            dock_area.update(cx, |dock_area, cx| {
                dock_area.toggle_dock(placement, window, cx);
            });
        }
    }
}

impl Render for PikuTitleBar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (left_open, right_open) = self
            .dock_area
            .upgrade()
            .map(|dock_area| {
                let dock_area = dock_area.read(cx);
                (
                    dock_area.is_dock_open(DockPlacement::Left, cx),
                    dock_area.is_dock_open(DockPlacement::Right, cx),
                )
            })
            .unwrap_or((true, true));

        TitleBar::new()
            .child(div().flex().items_center().pl_1().child(PikuLogo::new()))
            .child(
                h_flex()
                    .items_center()
                    .justify_end()
                    .px_2()
                    .gap_1()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        Button::new("new-tab")
                            .icon(IconName::Plus)
                            .small()
                            .ghost()
                            .tooltip("New tab (Ctrl+T)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(NewTab), cx);
                            }),
                    )
                    .child(
                        Button::new("split-right")
                            .icon(PikuIcon::Columns2)
                            .small()
                            .ghost()
                            .tooltip("Split right (Ctrl+Shift+E)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(SplitRight), cx);
                            }),
                    )
                    .child(
                        Button::new("split-down")
                            .icon(PikuIcon::Rows2)
                            .small()
                            .ghost()
                            .tooltip("Split down (Ctrl+Shift+O)")
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(SplitDown), cx);
                            }),
                    )
                    .child(
                        div()
                            .w(px(1.))
                            .h(px(16.))
                            .mx_1()
                            .bg(cx.theme().border),
                    )
                    .child(
                        Button::new("toggle-left-dock")
                            .icon(if left_open {
                                IconName::PanelLeftClose
                            } else {
                                IconName::PanelLeftOpen
                            })
                            .small()
                            .ghost()
                            .tooltip("Toggle navigation (Ctrl+B)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_dock(DockPlacement::Left, window, cx);
                            })),
                    )
                    .child(
                        Button::new("toggle-right-dock")
                            .icon(if right_open {
                                IconName::PanelRightClose
                            } else {
                                IconName::PanelRightOpen
                            })
                            .small()
                            .ghost()
                            .tooltip("Toggle details (Ctrl+Alt+B)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_dock(DockPlacement::Right, window, cx);
                            })),
                    )
                    .child(
                        Button::new("app-menu")
                            .icon(IconName::EllipsisVertical)
                            .small()
                            .ghost()
                            .dropdown_menu(move |menu, _, cx| {
                                let show_hidden =
                                    PikuState::global(cx).settings.read(cx).show_hidden;
                                menu.menu_with_check(
                                    "Show hidden files",
                                    show_hidden,
                                    Box::new(ToggleHidden),
                                )
                                .separator()
                                .menu("About PIKU", Box::new(ShowAbout))
                            }),
                    ),
            )
    }
}

/// The about card shown from the app menu.
#[allow(dead_code)]
pub fn about_content(cx: &App) -> impl IntoElement {
    v_flex()
        .items_center()
        .gap_3()
        .py_4()
        .child(PikuLogo::new().icon_size(px(56.)).icon_only())
        .child(
            div()
                .text_lg()
                .text_color(cx.theme().foreground)
                .child("PIKU"),
        )
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("Monochrome file manager · v{}", env!("CARGO_PKG_VERSION"))),
        )
}
