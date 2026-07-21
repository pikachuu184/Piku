//! Explorer pane toolbar: navigation, breadcrumbs, filter, sort, view.

use std::path::PathBuf;

use gpui::{
    Context, InteractiveElement as _, IntoElement, ParentElement,
    StatefulInteractiveElement as _, Styled, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, Size,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    menu::DropdownMenu as _,
};

use crate::app::actions::{SortByModified, SortByName, SortBySize, SortByType};
use crate::app::assets::PikuIcon;
use crate::state::pane_state::{SortBy, ViewMode};
use crate::ui::components::piku_spinner;
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
                // During a silent refresh (watcher-triggered reload over an
                // already-populated listing) the refresh button becomes a
                // spinner of the same footprint — no layout shift.
                if self.loading && !self.entries.is_empty() {
                    div()
                        .size(px(26.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .child(piku_spinner(Size::XSmall, cx))
                        .into_any_element()
                } else {
                    Button::new("nav-refresh")
                        .icon(PikuIcon::RefreshCw)
                        .xsmall()
                        .ghost()
                        .tooltip("Refresh (F5)")
                        .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx)))
                        .into_any_element()
                },
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_2()
                    .child(self.render_breadcrumbs(segments, last_ix, cx)),
            )
            // Input size tiers across PIKU: Small (30px) for toolbar inputs,
            // Large (40px) for dialog inputs. Keep new inputs on one of these.
            .child(
                div()
                    .id("filter-wrap")
                    .w(px(220.))
                    .max_w(px(280.))
                    .flex_shrink(1.)
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new("Filter this folder (Ctrl+F)")
                            .build(window, cx)
                    })
                    .child(Input::new(&self.filter_input).small().cleanable(true)),
            )
            .child(
                // Toggle recursive search: the text box then searches subfolders
                // instead of filtering the current listing.
                Button::new("search-toggle")
                    .icon(IconName::Search)
                    .xsmall()
                    .ghost()
                    .selected(self.is_deep_search())
                    .tooltip("Search subfolders")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_deep_search(window, cx);
                    })),
            )
            .child(
                // The active sort field is spelled out so the control reads at
                // a glance; the icon carries the direction.
                Button::new("sort-menu")
                    .icon(if ascending {
                        IconName::SortAscending
                    } else {
                        IconName::SortDescending
                    })
                    .label(match sort {
                        SortBy::Name => "Name",
                        SortBy::Size => "Size",
                        SortBy::Modified => "Modified",
                        SortBy::Type => "Type",
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
                Button::new("new-file")
                    .icon(PikuIcon::FilePlus)
                    .xsmall()
                    .ghost()
                    .tooltip("New file (Ctrl+N)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_create(false, window, cx);
                    })),
            )
            .child(
                Button::new("new-folder")
                    .icon(PikuIcon::FolderPlus)
                    .xsmall()
                    .ghost()
                    .tooltip("New folder (Ctrl+Shift+N)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.start_create(true, window, cx);
                    })),
            )
    }

    /// Custom breadcrumb row: icon + label per segment (the library's
    /// `BreadcrumbItem` is label-only). Deep paths collapse to
    /// `first › … › last three` so the filter input never overflows.
    fn render_breadcrumbs(
        &self,
        segments: Vec<(String, PathBuf)>,
        last_ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // None marks the ellipsis gap in a collapsed deep path.
        let visible: Vec<Option<(usize, String, PathBuf)>> = if segments.len() > 6 {
            let mut items: Vec<Option<(usize, String, PathBuf)>> = Vec::with_capacity(5);
            let mut iter = segments.into_iter().enumerate();
            let first = iter.next().map(|(ix, (label, path))| Some((ix, label, path)));
            items.extend(first);
            items.push(None);
            let rest: Vec<_> = iter.collect();
            items.extend(
                rest.into_iter()
                    .skip(last_ix.saturating_sub(3))
                    .map(|(ix, (label, path))| Some((ix, label, path))),
            );
            items
        } else {
            segments
                .into_iter()
                .enumerate()
                .map(|(ix, (label, path))| Some((ix, label, path)))
                .collect()
        };

        let mut row = h_flex().items_center().gap_0p5().overflow_hidden();
        let count = visible.len();
        for (pos, item) in visible.into_iter().enumerate() {
            match item {
                Some((ix, label, path)) => {
                    let is_last = ix == last_ix;
                    row = row.child(
                        Button::new(("crumb", ix))
                            .icon(
                                Icon::new(if ix == 0 {
                                    IconName::HardDrive
                                } else {
                                    IconName::Folder
                                })
                                .size(px(13.)),
                            )
                            .label(label)
                            .xsmall()
                            .ghost()
                            .disabled(is_last)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.navigate_to(path.clone(), window, cx);
                            })),
                    );
                }
                None => {
                    row = row.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("…"),
                    );
                }
            }
            if pos + 1 < count {
                row = row.child(
                    Icon::new(IconName::ChevronRight)
                        .size(px(12.))
                        .text_color(cx.theme().muted_foreground),
                );
            }
        }
        row
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
