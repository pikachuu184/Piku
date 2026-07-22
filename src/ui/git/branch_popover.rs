//! Branch panel anchored above the status bar: local/remote branch list,
//! checkout on click, inline branch creation (no modal — the workspace
//! switcher's inline-edit pattern), and a two-click guarded delete.
//! Push renders as a visibly disabled row: the pure-Rust backend does not
//! support it yet, and pretending otherwise would be worse.

use std::path::PathBuf;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, input::InputState, v_flex};

use crate::security::git_text::validate_branch_name;
use crate::services::git::types::BranchInfo;
use crate::state::PikuState;

pub struct BranchPopover {
    root: PathBuf,
    branches: Vec<BranchInfo>,
    loading: bool,
    /// Branch name pending delete confirmation (second click executes).
    confirm_delete: Option<String>,
    /// Inline "create branch" editor.
    create_input: Entity<InputState>,
    /// Local validation error (store errors render in the git inspector).
    error: Option<String>,
    /// Mutation epoch + head branch at the last reload — the list only
    /// reloads when one of them changes, not on every store notify.
    seen_epoch: u64,
    seen_head: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl BranchPopover {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let create_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("New branch name…"));
        let git = PikuState::global(cx).git.clone();
        let git_sub = cx.observe(&git, |this: &mut Self, git, cx| {
            // Reload only when something branch-shaped actually changed —
            // a status refresh alone must not respawn a branches() walk.
            let store = git.read(cx);
            let epoch = store.epoch();
            let head = store
                .snapshot(&this.root)
                .and_then(|snap| snap.branch.clone());
            if epoch != this.seen_epoch || head != this.seen_head {
                this.seen_epoch = epoch;
                this.seen_head = head;
                this.reload(cx);
            }
            cx.notify();
        });

        let (seen_epoch, seen_head) = {
            let store = git.read(cx);
            (
                store.epoch(),
                store.snapshot(&root).and_then(|snap| snap.branch.clone()),
            )
        };
        let mut this = Self {
            root,
            branches: Vec::new(),
            loading: false,
            confirm_delete: None,
            create_input,
            error: None,
            seen_epoch,
            seen_head,
            _subscriptions: vec![git_sub],
        };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        let backend = PikuState::global(cx).git.read(cx).backend();
        let root = self.root.clone();
        let task = cx.background_executor().spawn(async move {
            use crate::services::git::backend::GitBackend as _;
            backend.branches(&root)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.loading = false;
                if let Ok(branches) = result {
                    this.branches = branches;
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn on_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.create_input.read(cx).value().trim().to_string();
        if let Err(reason) = validate_branch_name(&name) {
            self.error = Some(reason);
            cx.notify();
            return;
        }
        self.error = None;
        let root = self.root.clone();
        PikuState::global(cx)
            .git
            .clone()
            .update(cx, |git, cx| git.create_branch(root, name, cx));
        self.create_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }
}

impl Render for BranchPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let locals: Vec<BranchInfo> = self
            .branches
            .iter()
            .filter(|b| !b.is_remote)
            .cloned()
            .collect();
        let remotes: Vec<BranchInfo> = self
            .branches
            .iter()
            .filter(|b| b.is_remote)
            .cloned()
            .collect();

        let label = |text: &'static str| {
            div()
                .px_2()
                .pt_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(text)
        };

        let mut panel = v_flex()
            .w(px(280.))
            .max_h(px(400.))
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar)
            .shadow_none()
            .overflow_hidden()
            .child(
                // Inline create row.
                h_flex()
                    .gap_1()
                    .p_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(gpui_component::input::Input::new(&self.create_input).small()),
                    )
                    .child(
                        div()
                            .id("branch-create")
                            .px_2()
                            .py_1()
                            .text_xs()
                            .flex_none()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .bg(cx.theme().list_active)
                            .text_color(cx.theme().foreground)
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .child("Create")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_create(window, cx);
                            })),
                    ),
            );

        if let Some(error) = &self.error {
            panel = panel.child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(IconName::TriangleAlert).size(px(12.)))
                    .child(error.clone()),
            );
        }

        let mut list = v_flex()
            .id("branch-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .pb_1();

        list = list.child(label("Local"));
        if locals.is_empty() && self.loading {
            list = list.child(label("Loading…"));
        }
        for (ix, branch) in locals.iter().enumerate() {
            let name = branch.name.clone();
            let is_head = branch.is_head;
            // Operations key off the real ref name, so branches whose
            // display form was sanitized stay fully operable.
            let confirming = self.confirm_delete.as_deref() == Some(branch.ref_name.as_str());
            let checkout_name = branch.ref_name.clone();
            let delete_name = branch.ref_name.clone();
            list = list.child(
                h_flex()
                    .id(gpui::SharedString::from(format!("branch-{ix}")))
                    .mx_1()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .when(is_head, |s| s.bg(cx.theme().list_active))
                    .when(!is_head, |s| s.hover(|s| s.bg(cx.theme().list_hover)))
                    .child(
                        div().w(px(14.)).flex_none().when(is_head, |slot| {
                            slot.child(
                                Icon::new(IconName::Check)
                                    .size(px(12.))
                                    .text_color(cx.theme().foreground),
                            )
                        }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .text_color(cx.theme().foreground)
                                    .child(name.clone()),
                            )
                            .when_some(branch.upstream.clone(), |col, upstream| {
                                col.child(
                                    h_flex()
                                        .gap_0p5()
                                        .items_center()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(Icon::new(IconName::ArrowUp).size(px(10.)))
                                        .child(div().truncate().child(upstream)),
                                )
                            }),
                    )
                    .when(!is_head, |row| {
                        row.child(
                            div()
                                .id(gpui::SharedString::from(format!("branch-del-{ix}")))
                                .px_1p5()
                                .text_xs()
                                .flex_none()
                                .rounded(cx.theme().radius)
                                .cursor_pointer()
                                .text_color(cx.theme().muted_foreground)
                                .hover(|s| {
                                    s.bg(cx.theme().list_hover)
                                        .text_color(cx.theme().foreground)
                                })
                                .map(|button| {
                                    if confirming {
                                        button.child("delete?")
                                    } else {
                                        button.child(Icon::new(IconName::Close).size(px(11.)))
                                    }
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.confirm_delete.as_deref()
                                        == Some(delete_name.as_str())
                                    {
                                        this.confirm_delete = None;
                                        let root = this.root.clone();
                                        let name = delete_name.clone();
                                        PikuState::global(cx)
                                            .git
                                            .clone()
                                            .update(cx, |git, cx| {
                                                git.delete_branch(root, name, cx)
                                            });
                                    } else {
                                        this.confirm_delete = Some(delete_name.clone());
                                    }
                                    cx.notify();
                                })),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.confirm_delete = None;
                            let root = this.root.clone();
                            let name = checkout_name.clone();
                            PikuState::global(cx)
                                .git
                                .clone()
                                .update(cx, |git, cx| git.checkout(root, name, cx));
                            cx.notify();
                        }))
                    }),
            );
        }

        if !remotes.is_empty() {
            list = list.child(label("Remote"));
            for branch in &remotes {
                list = list.child(
                    div()
                        .mx_1()
                        .px_2()
                        .py_0p5()
                        .text_xs()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(branch.name.clone()),
                );
            }
        }

        panel = panel.child(list).child(
            // Footer: capabilities that are visible but honest about limits.
            div()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Push — not yet supported"),
        );
        panel
    }
}
