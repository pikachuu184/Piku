//! Workspace name prompt dialog (create / rename), matching the explorer
//! dialog pattern: Large input tier, Cancel + primary footer, validation
//! errors surfaced as notifications without closing the dialog.

use gpui::{App, AppContext as _, ParentElement, Styled, Window, div, px};
use gpui_component::{
    Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    input::{Input, InputState},
    notification::Notification,
};

/// Open a dialog asking for a workspace name. `on_submit` returns `Err` with
/// a user-presentable message to keep the dialog open (e.g. duplicate name).
pub fn prompt_workspace_name(
    title: &'static str,
    ok_label: &'static str,
    initial: String,
    on_submit: impl Fn(String, &mut Window, &mut App) -> Result<(), String> + 'static,
    window: &mut Window,
    cx: &mut App,
) {
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(initial)
            .placeholder("Workspace name")
    });
    let on_submit = std::rc::Rc::new(on_submit);

    window.open_dialog(cx, move |dialog, _, _| {
        let input = input.clone();
        let ok_input = input.clone();
        let on_submit = on_submit.clone();
        dialog
            .title(title)
            .w(px(380.))
            .content(move |content, _, _| {
                content.child(div().py_2().child(Input::new(&input).large().cleanable(true)))
            })
            .footer(
                DialogFooter::new()
                    .pb_4()
                    .px_4()
                    .child(
                        DialogClose::new()
                            .child(Button::new("ws-dialog-cancel").label("Cancel").outline().small()),
                    )
                    .child(
                        DialogAction::new().child(
                            Button::new("ws-dialog-ok").label(ok_label).primary().small(),
                        ),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let name = ok_input.read(cx).value().trim().to_string();
                match on_submit(name, window, cx) {
                    Ok(()) => true,
                    Err(error) => {
                        window.push_notification(Notification::error(error), cx);
                        false
                    }
                }
            })
    });
}
