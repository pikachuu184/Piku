//! Explorer pane toolbar: navigation, breadcrumbs, filter, sort, view.

use std::path::PathBuf;

use gpui::{Context, IntoElement, ParentElement, Styled, Window, div, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::DropdownMenu as _,
};

use crate::app::actions::{SortByModified, SortByName, SortBySize, SortByType};
use crate::app::assets::PikuIcon;
use crate::state::pane_state::{SortBy, ViewMode};
use crate::ui::explorer::ExplorerPanel;

impl ExplorerPanel {
    pub(super) fn render_toolbar(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sort = self.session.sort;
        let ascending = self.session.ascending;

        // Breadcrumb segments: drive prefix plus each directory component.
        let mut segments: Vec<(String, PathBuf)> = Vec::new();
        let mut acc = PathBuf::new();
        for component in self.session.cwd.components() {
            use std::path::Component;
            match component {
                Component::Prefix(prefix) => {
                    acc.push(component.as_os_str());
                    acc.push("\\");
                    segments.push((
                        prefix.as_os_str().to_string_lossy().into_owned(),
                        acc.clone(),
                    ));
                }
                Component::RootDir => {}
                _ => {
                    acc.push(component.as_os_str());
                    segments.push((
                        component.as_os_str().to_string_lossy().into_owned(),
                        acc.clone(),
                    ));
                }
            }
        }
        let last_ix = segments.len().saturating_sub(1);

        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("nav-back")
                    .icon(IconName::ArrowLeft)
                    .xsmall()
                    .ghost()
                    .disabled(self.back_stack.is_empty())
                    .tooltip("Back (Alt+Left)")
                    .on_click(cx.listener(|this, _, window, cx| this.go_back(window, cx))),
            )
            .child(
                Button::new("nav-fwd")
                    .icon(IconName::ArrowRight)
                    .xsmall()
                    .ghost()
                    .disabled(self.fwd_stack.is_empty())
                    .tooltip("Forward (Alt+Right)")
                    .on_click(cx.listener(|this, _, window, cx| this.go_forward(window, cx))),
            )
            .child(
                Button::new("nav-up")
                    .icon(IconName::ArrowUp)
                    .xsmall()
                    .ghost()
                    .disabled(self.session.cwd.parent().is_none())
                    .tooltip("Up (Alt+Up)")
                    .on_click(cx.listener(|this, _, window, cx| this.go_up(window, cx))),
            )
            .child(
                Button::new("nav-refresh")
                    .icon(PikuIcon::RefreshCw)
                    .xsmall()
                    .ghost()
                    .tooltip("Refresh (F5)")
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .child(
                div().flex_1().min_w_0().px_2().child(
                    Breadcrumb::new().children(segments.into_iter().enumerate().map(
                        |(ix, (label, path))| {
                            BreadcrumbItem::new(label)
                                .disabled(ix == last_ix)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.navigate_to(path.clone(), window, cx);
                                }))
                        },
                    )),
                ),
            )
            // Input size tiers across PIKU: Small (30px) for toolbar inputs,
            // Large (40px) for dialog inputs. Keep new inputs on one of these.
            .child(
                div()
                    .w(px(220.))
                    .max_w(px(280.))
                    .flex_shrink(1.)
                    .child(Input::new(&self.filter_input).small().cleanable(true)),
            )
            .child(
                Button::new("sort-menu")
                    .icon(if ascending {
                        IconName::SortAscending
                    } else {
                        IconName::SortDescending
                    })
                    .xsmall()
                    .ghost()
                    .tooltip("Sort by")
                    .dropdown_menu(move |menu, _, _| {
                        menu.menu_with_check("Name", sort == SortBy::Name, Box::new(SortByName))
                            .menu_with_check("Size", sort == SortBy::Size, Box::new(SortBySize))
                            .menu_with_check(
                                "Modified",
                                sort == SortBy::Modified,
                                Box::new(SortByModified),
                            )
                            .menu_with_check("Type", sort == SortBy::Type, Box::new(SortByType))
                    }),
            )
            .child(
                Button::new("view-toggle")
                    .icon(match self.session.view {
                        ViewMode::List => PikuIcon::LayoutGrid,
                        ViewMode::Grid => PikuIcon::List,
                    })
                    .xsmall()
                    .ghost()
                    .tooltip(match self.session.view {
                        ViewMode::List => "Grid view (Ctrl+Shift+V)",
                        ViewMode::Grid => "List view (Ctrl+Shift+V)",
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        let _ = window;
                        this.dispatch_toggle_view(cx);
                    })),
            )
            .child(
                Button::new("new-folder")
                    .icon(PikuIcon::FolderPlus)
                    .xsmall()
                    .ghost()
                    .tooltip("New folder (Ctrl+Shift+N)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        super::dialogs::new_folder(this, window, cx);
                    })),
            )
    }

    pub(super) fn dispatch_toggle_view(&mut self, cx: &mut Context<Self>) {
        use crate::state::pane_state::ViewMode;
        self.session.view = match self.session.view {
            ViewMode::List => ViewMode::Grid,
            ViewMode::Grid => ViewMode::List,
        };
        cx.notify();
    }
}
