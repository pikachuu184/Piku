//! Confirmation dialogs. Creation and rename are inline editors in the
//! explorer pane (see `explorer_panel::InlineEdit`) — only destructive or
//! risky flows (delete, running an executable) stay modal on purpose.

use gpui::{ParentElement, Window};
use gpui_component::{WindowExt as _, dialog::DialogButtonProps, v_flex};

use crate::core::entry::FsEntry;
use crate::state::PikuState;

pub(super) fn confirm_delete(entries: Vec<FsEntry>, window: &mut Window, cx: &mut gpui::App) {
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

/// Shell-opening an executable or script runs it — confirm before handing it
/// to the OS. The actual open goes through the hardened
/// [`super::explorer_panel::shell_open`] (canonicalize + re-authorize).
pub(super) fn confirm_open_executable(entry: FsEntry, window: &mut Window, cx: &mut gpui::App) {
    let description = format!(
        "“{}” is a program or script. Opening it will run it on this computer.",
        entry.name
    );
    window.open_alert_dialog(cx, move |alert, _, _| {
        let entry = entry.clone();
        alert
            .confirm()
            .title("Run this file?")
            .description(v_flex().child(description.clone()))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Run")
                    .cancel_text("Cancel")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                super::explorer_panel::shell_open(&entry.name, &entry.path, window, cx);
                true
            })
    });
}
