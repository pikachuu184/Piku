//! Confirmation dialogs. Creation and rename are inline editors in the
//! explorer pane (see `explorer_panel::InlineEdit`) — only destructive or
//! risky flows (delete, running an executable) stay modal on purpose.

use gpui::{ParentElement, Window};
use gpui_component::{WindowExt as _, dialog::DialogButtonProps, v_flex};

use crate::core::entry::FsEntry;
use crate::services::jobs::label;
use crate::state::PikuState;

/// Move to the trash — the reversible one, and the one the `delete` key runs.
///
/// Skippable: `settings.confirm_delete` governs this dialog, because what it is
/// confirming is recoverable from the Recycle Bin.
pub(super) fn confirm_delete(entries: Vec<FsEntry>, window: &mut Window, cx: &mut gpui::App) {
    // Both label builders name the first entry, so an empty selection has no
    // sentence to write. Callers already guard; this is so a future one cannot
    // turn "nothing selected" into a panic on a destructive path.
    let Some(first) = entries.first() else {
        return;
    };
    let count = entries.len();
    let description = label::delete_description(&first.name, count);
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

/// Delete permanently — no trash, no undo.
///
/// Its own dialog rather than a flag on [`confirm_delete`], and deliberately not
/// skippable: `settings.confirm_delete` is the user's answer about the
/// *reversible* flow, and reading it here would let a preference set for "I know
/// where the Recycle Bin is" silently authorize destruction. Every string says
/// "permanently", including the OK button, since the only real defence against
/// clicking through is that it does not look like the dialog above.
pub(super) fn confirm_delete_permanent(
    entries: Vec<FsEntry>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let Some(first) = entries.first() else {
        return;
    };
    let count = entries.len();
    let description = label::delete_permanent_description(&first.name, count);
    let paths: Vec<std::path::PathBuf> = entries.iter().map(|e| e.path.clone()).collect();

    window.open_alert_dialog(cx, move |alert, _, _| {
        let paths = paths.clone();
        alert
            .confirm()
            .title("Delete permanently?")
            .description(v_flex().child(description.clone()))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Delete permanently")
                    .cancel_text("Cancel")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let paths = paths.clone();
                PikuState::global(cx).jobs.clone().update(cx, |jobs, cx| {
                    jobs.submit_delete_permanent(paths, window, cx);
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
