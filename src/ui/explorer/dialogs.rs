//! Rename, new-folder, and delete-confirmation dialogs.

use gpui::{AppContext as _, Context, ParentElement, Styled, Window, div, px};
use gpui_component::{
    Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogButtonProps, DialogClose, DialogFooter},
    input::{Input, InputState},
    notification::Notification,
    v_flex,
};

use crate::core::entry::FsEntry;
use crate::security::file_name::validate_name;
use crate::state::PikuState;
use crate::ui::explorer::ExplorerPanel;

/// Standard dialog footer: Cancel + primary action. Enter/Escape also work
/// through the dialog's built-in key bindings.
fn action_footer(action_label: &'static str) -> DialogFooter {
    DialogFooter::new()
        .pb_4()
        .px_4()
        .child(
            DialogClose::new().child(Button::new("dialog-cancel").label("Cancel").outline().small()),
        )
        .child(
            DialogAction::new().child(
                Button::new("dialog-ok")
                    .label(action_label)
                    .primary()
                    .small(),
            ),
        )
}

pub(super) fn rename_selected(
    panel: &mut ExplorerPanel,
    window: &mut Window,
    cx: &mut Context<ExplorerPanel>,
) {
    let selected = panel.selected_entries();
    let Some(entry) = selected.first().cloned() else {
        return;
    };
    if selected.len() > 1 {
        window.push_notification(Notification::info("Select a single item to rename"), cx);
        return;
    }

    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value(entry.name.clone())
            .placeholder("New name")
    });
    let old_path = entry.path.clone();

    window.open_dialog(cx, move |dialog, _, _| {
        let input = input.clone();
        let ok_input = input.clone();
        let old_path = old_path.clone();
        dialog
            .title("Rename")
            .w(px(380.))
            .content(move |content, _, _| {
                content.child(div().py_2().child(Input::new(&input)))
            })
            .footer(action_footer("Rename"))
            .on_ok(move |_, window, cx| {
                let new_name = ok_input.read(cx).value().trim().to_string();
                if let Err(error) = validate_name(&new_name) {
                    window.push_notification(Notification::error(error), cx);
                    return false;
                }
                let Some(parent) = old_path.parent() else {
                    return true;
                };
                let new_path = parent.join(&new_name);
                if new_path == old_path {
                    return true;
                }
                let from = old_path.clone();
                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                    jobs.submit_rename(from, new_path, window, cx);
                });
                true
            })
    });
}

pub(super) fn new_folder(
    panel: &mut ExplorerPanel,
    window: &mut Window,
    cx: &mut Context<ExplorerPanel>,
) {
    let dir = panel.cwd().clone();
    let input = cx.new(|cx| {
        InputState::new(window, cx)
            .default_value("New folder")
            .placeholder("Folder name")
    });

    window.open_dialog(cx, move |dialog, _, _| {
        let input = input.clone();
        let ok_input = input.clone();
        let dir = dir.clone();
        dialog
            .title("New folder")
            .w(px(380.))
            .content(move |content, _, _| {
                content.child(div().py_2().child(Input::new(&input)))
            })
            .footer(action_footer("Create"))
            .on_ok(move |_, window, cx| {
                let name = ok_input.read(cx).value().trim().to_string();
                if let Err(error) = validate_name(&name) {
                    window.push_notification(Notification::error(error), cx);
                    return false;
                }
                let path = dir.join(&name);
                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                    jobs.submit_new_folder(path, window, cx);
                });
                true
            })
    });
}

pub(super) fn confirm_delete(
    entries: Vec<FsEntry>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let count = entries.len();
    let description = if count == 1 {
        format!("“{}” will be moved to the Recycle Bin.", entries[0].name)
    } else {
        format!("{count} items will be moved to the Recycle Bin.")
    };
    let paths: Vec<std::path::PathBuf> = entries.iter().map(|e| e.path.clone()).collect();

    window.open_alert_dialog(cx, move |alert, _, _| {
        let paths = paths.clone();
        alert
            .confirm()
            .title("Move to Recycle Bin?")
            .description(v_flex().child(description.clone()))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Move to Recycle Bin")
                    .cancel_text("Cancel")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let paths = paths.clone();
                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                    jobs.submit_delete(paths, window, cx);
                });
                true
            })
    });
}
