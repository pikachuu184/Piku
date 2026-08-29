//! One job, as a card.
//!
//! The card is the only place in the app that shows a transfer in full, and it
//! shows completion and activity as two separate things: the ring fills toward
//! 100% and the waveform says whether bytes are moving *now*. A single bar cannot
//! say both — a copy stalled at 60% for four minutes and one flying through 60%
//! draw the same bar — which is the whole reason there are two widgets here.
//!
//! # Rows
//!
//! 1. Ring, title, status word, controls.
//! 2. Identity: `assets/ → /backup/assets/`.
//! 3. Waveform, when the job has been sampled at all.
//! 4. The item in flight, while one is.
//! 5. The numeric row, from [`label::progress_line`].
//! 6. The failure list, when expanded.
//!
//! Rows 3 and 4 are dropped rather than drawn empty. A queued job has no samples
//! and a rename never will have any, and a row of baseline bars would be a claim
//! that a reading exists.
//!
//! # Why the detail element is a parameter
//!
//! Row 6 is a [`v_virtual_list`](gpui_component::v_virtual_list), which needs the
//! hosting entity and a scroll handle. This function is generic over its host view
//! so the same card can be rendered from anywhere, so it cannot build one — the
//! transfer center passes it in. `detail.is_some()` *is* the expanded state; there
//! is no second flag to disagree with it.
//!
//! Theme rules, as everywhere else on this surface: `cx.theme().*` tokens only,
//! `cx.theme().radius` corners, `.ghost().xsmall()` buttons with tooltips, no
//! shadows, no hues.

use gpui::{
    AnyElement, App, ClickEvent, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{DropdownMenu as _, PopupMenuItem},
    progress::ProgressCircle,
    v_flex,
};

use crate::app::assets::PikuIcon;
use crate::backend::services::transfer::job::{Priority, TransferFailure};
use crate::security::text::sanitize_path;
use crate::services::jobs::{Job, JobStatus, label};
use crate::ui::transfers::waveform::{CARD_HEIGHT, waveform};
use crate::ui::transfers::with_jobs;

/// Diameter of the completion ring, in pixels.
const RING: f32 = 26.0;

/// Height of one row in the expanded failure list.
///
/// Shared with the transfer center, which needs it to size the virtual list. One
/// constant rather than two that must agree: a mismatch here is rows that overlap
/// or drift, and neither errors.
pub const DETAIL_ROW_HEIGHT: f32 = 22.0;

/// Whether this job has anything worth expanding.
///
/// Called by the card (to decide whether to draw the chevron) and by the transfer
/// center (to decide whether to build the list), so the two cannot disagree about
/// whether a chevron leads anywhere.
pub fn expandable(job: &Job) -> bool {
    !job.failures.is_empty()
}

/// One row of the expanded failure list: the item, and why it was stepped over.
///
/// Both strings were sanitized by the engine before they reached [`Job`] — see
/// [`TransferFailure`] — so nothing here re-derives them from a path.
pub fn failure_row(failure: &TransferFailure, cx: &App) -> AnyElement {
    h_flex()
        .h(px(DETAIL_ROW_HEIGHT))
        .w_full()
        .gap_2()
        .items_center()
        .text_xs()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(cx.theme().foreground)
                .child(failure.path.to_string()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(cx.theme().muted_foreground)
                .child(failure.detail.to_string()),
        )
        .into_any_element()
}

/// `assets/ + 2 more → /backup/assets/`.
///
/// The leaf for the source, because the card's title already named the operation
/// and a full path repeated twice fills the row with prefix nobody reads; the full
/// path for the destination, because that is the part the user is checking. Both go
/// through the sanitizers — [`label::file_label`] and [`sanitize_path`] — so an
/// invisible or reordering character in a filename cannot make this row read as a
/// different path than the one being written.
fn identity(job: &Job, cx: &App) -> AnyElement {
    let source = match job.sources.split_first() {
        None => return div().into_any_element(),
        Some((first, [])) => label::file_label(first),
        Some((first, rest)) => format!("{} + {} more", label::file_label(first), rest.len()),
    };

    let mut row = h_flex()
        .w_full()
        .gap_1()
        .items_center()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(div().flex_none().max_w(px(180.)).truncate().child(source));

    if let Some(destination) = job.destination.as_deref() {
        row = row
            .child(
                Icon::new(IconName::ArrowRight)
                    .size(px(11.))
                    .flex_none()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(sanitize_path(destination)),
            );
    }
    row.into_any_element()
}

/// The kebab menu: everything that is not one of the hot buttons.
///
/// Takes no context: every item's handler is given its own `window` and `cx` when
/// the menu is built, which is what lets these be plain closures instead of
/// data-carrying actions.
fn kebab(job: &Job) -> AnyElement {
    let id = job.id;
    let destination = job.destination.clone();
    let can_retry = job.can_retry();
    let reconfirm = job.kind.needs_reconfirm();
    let error = job.error().map(str::to_owned);
    let finished = job.status.is_terminal();

    Button::new(SharedString::from(format!("job-{id}-more")))
        .icon(IconName::EllipsisVertical)
        .xsmall()
        .ghost()
        .tooltip("More")
        .dropdown_menu(move |menu, window, cx| {
            let mut menu = menu;

            if let Some(destination) = destination.clone() {
                let navigate = destination.clone();
                menu = menu.item(
                    PopupMenuItem::new("Show in folder")
                        .icon(Icon::new(PikuIcon::FolderOpen))
                        .on_click(move |_, window, cx| {
                            // Through the explorer's own navigation, which
                            // re-authorizes the path — not a shell handler, and not
                            // a new window.
                            crate::ui::explorer::navigate_active(navigate.clone(), window, cx);
                        }),
                );
                menu = menu.item(
                    PopupMenuItem::new("Copy destination")
                        .icon(Icon::new(IconName::Copy))
                        .on_click(move |_, _, cx| {
                            // The sanitized form, not the raw one. The clipboard is
                            // a display surface belonging to some other program —
                            // often a shell — and a path that cannot be shown
                            // safely here is not one to hand over there either.
                            // PIKU's own paste uses `FileClipboard`, which carries
                            // real `PathBuf`s, so nothing in the app depends on
                            // this text being byte-exact.
                            cx.write_to_clipboard(gpui::ClipboardItem::new_string(sanitize_path(
                                &destination,
                            )));
                        }),
                );
                menu = menu.separator();
            }

            if can_retry {
                menu = menu.item(
                    PopupMenuItem::new("Retry")
                        .icon(Icon::new(IconName::Redo))
                        .on_click(move |_, window, cx| {
                            // A retry re-submits the original request, and for a
                            // permanent delete that means re-running destruction
                            // from a menu — so it asks again rather than becoming
                            // the quiet way around `confirm_delete_permanent`.
                            if reconfirm {
                                confirm_retry(id, window, cx);
                            } else {
                                with_jobs(cx, |jobs, cx| {
                                    jobs.retry(id, window, cx);
                                });
                            }
                        }),
                );
            }

            if let Some(error) = error.clone() {
                menu = menu.item(
                    PopupMenuItem::new("Open error details")
                        .icon(Icon::new(IconName::TriangleAlert))
                        .on_click(move |_, window, cx| error_details(&error, window, cx)),
                );
            }

            if !finished {
                menu = menu
                    .separator()
                    .submenu("Prioritize", window, cx, move |menu, _, _| {
                        let mut menu = menu;
                        for priority in [
                            Priority::Interactive,
                            Priority::Normal,
                            Priority::Background,
                        ] {
                            menu = menu.item(PopupMenuItem::new(priority.label()).on_click(
                                move |_, _, cx| {
                                    with_jobs(cx, |jobs, cx| jobs.prioritize(id, priority, cx));
                                },
                            ));
                        }
                        menu
                    });
            }

            if finished {
                menu = menu.separator().item(
                    PopupMenuItem::new("Remove from history")
                        .icon(Icon::new(PikuIcon::Trash))
                        .on_click(move |_, _, cx| {
                            with_jobs(cx, |jobs, cx| jobs.remove(id, cx));
                        }),
                );
            }

            menu
        })
        .into_any_element()
}

/// The job-level error in full, for when the card truncated it.
///
/// A second dialog stacking on the transfer center is what the dialog layer is
/// built for — see `Root::active_dialogs` — so this does not have to close
/// anything first.
fn error_details(error: &str, window: &mut Window, cx: &mut App) {
    use gpui_component::WindowExt as _;
    let error = error.to_owned();
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .title("Transfer failed")
            .description(v_flex().child(error.clone()))
    });
}

/// Ask again before re-running a job whose kind destroys data irrecoverably.
///
/// The same shape as [`error_details`] — an alert stacked on the center — and the
/// same wording as the explorer's own permanent-delete dialog, since a user who
/// recognizes one should recognize the other.
fn confirm_retry(id: u64, window: &mut Window, cx: &mut App) {
    use gpui_component::{WindowExt as _, dialog::DialogButtonProps};
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .confirm()
            .title(label::RETRY_PERMANENT_TITLE)
            .description(v_flex().child(label::RETRY_PERMANENT_BODY))
            .button_props(
                DialogButtonProps::default()
                    .ok_text(label::RETRY_PERMANENT_OK)
                    .cancel_text("Cancel")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                with_jobs(cx, |jobs, cx| {
                    jobs.retry(id, window, cx);
                });
                true
            })
    });
}

/// One job's card. `detail` is row 6, and being `Some` is what "expanded" means.
pub fn card<V: 'static>(
    job: &Job,
    detail: Option<AnyElement>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &mut Context<V>,
) -> AnyElement {
    let id = job.id;
    let expanded = detail.is_some();
    let moving = job.is_moving();
    // No totals yet means the scan is still walking the tree, and a ring at 0%
    // would read as a stalled copy rather than as an unknown size.
    let scanning = moving && job.total_bytes == 0 && job.total_items == 0;

    let status = h_flex().flex_none().gap_1().items_center().text_xs().child(
        div()
            .text_color(if job.error().is_some() {
                // Grayscale emphasis, the same rule the git badges use: a
                // failure gets the brightest tier rather than a colour.
                cx.theme().foreground
            } else {
                cx.theme().muted_foreground
            })
            .child(label::status_label(&job.status)),
    );

    let mut controls = h_flex().flex_none().gap_0p5().items_center();
    if expandable(job) {
        controls = controls.child(
            Button::new(SharedString::from(format!("job-{id}-expand")))
                .icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .ghost()
                .tooltip(if expanded {
                    "Hide details"
                } else {
                    "Show details"
                })
                .on_click(on_toggle),
        );
    }
    // The one control that is a label rather than a glyph, because it is the one
    // the job cannot proceed without. A waiting job is stopped until a human
    // answers, and an icon among four other icons is not how that gets found.
    if job.status == JobStatus::WaitingForInput {
        controls = controls.child(
            Button::new(SharedString::from(format!("job-{id}-resolve")))
                .label(label::RESOLVE)
                .xsmall()
                .ghost()
                .tooltip("Answer this transfer's conflicts")
                .on_click(move |_, window, cx| {
                    crate::ui::transfers::conflicts::ConflictView::open(id, window, cx);
                }),
        );
    }
    if job.can_pause() {
        controls = controls.child(
            Button::new(SharedString::from(format!("job-{id}-pause")))
                .icon(PikuIcon::Pause)
                .xsmall()
                .ghost()
                .tooltip("Pause")
                .on_click(cx.listener(move |_, _, _, cx| {
                    with_jobs(cx, |jobs, cx| jobs.pause(id, cx));
                })),
        );
    }
    if job.can_resume() {
        controls = controls.child(
            Button::new(SharedString::from(format!("job-{id}-resume")))
                .icon(PikuIcon::Play)
                .xsmall()
                .ghost()
                .tooltip("Resume")
                .on_click(cx.listener(move |_, _, _, cx| {
                    with_jobs(cx, |jobs, cx| jobs.resume(id, cx));
                })),
        );
    }
    if job.can_cancel() {
        controls = controls.child(
            Button::new(SharedString::from(format!("job-{id}-cancel")))
                .icon(IconName::Close)
                .xsmall()
                .ghost()
                .tooltip("Cancel")
                .on_click(cx.listener(move |_, _, _, cx| {
                    with_jobs(cx, |jobs, cx| jobs.cancel(id, cx));
                })),
        );
    }
    controls = controls.child(kebab(job));

    let numbers = label::progress_line(job);
    let mut card = v_flex()
        .id(SharedString::from(format!("job-card-{id}")))
        .w_full()
        .p_2()
        .gap_1()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .child(
                    ProgressCircle::new(SharedString::from(format!("job-{id}-ring")))
                        .value(job.percent())
                        .loading(scanning)
                        .color(if moving {
                            // The same token the status bar's linear bar uses, so
                            // the two surfaces agree about what "in progress"
                            // looks like.
                            cx.theme().progress_bar
                        } else {
                            cx.theme().muted_foreground
                        })
                        // From `Styled`, and it wins: `ProgressCircle` applies its
                        // `Sizable` mapping first and refines style afterwards.
                        .size(px(RING))
                        .flex_none(),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(job.title.clone()),
                )
                .child(status)
                .child(controls),
        )
        .child(identity(job, cx));

    if !job.rate.is_empty() {
        card = card.child(waveform(&job.rate, CARD_HEIGHT, moving, cx));
    }
    if let Some(item) = job.current_item.clone().filter(|_| moving) {
        card = card.child(
            div()
                .w_full()
                .truncate()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(item.to_string()),
        );
    }
    if !numbers.is_empty() {
        card = card.child(
            h_flex()
                .w_full()
                .gap_2()
                .items_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(div().flex_1().min_w_0().truncate().child(numbers))
                .child(
                    div()
                        .flex_none()
                        .child(format!("{:.0}%", job.percent().clamp(0.0, 100.0))),
                ),
        );
    }
    // Not gated on `expandable`: a job whose failures were cleared while its card
    // was open has an empty list, and the note is what says so.
    if let Some(detail) = detail {
        card = card.child(
            v_flex()
                .w_full()
                .pt_1()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label::failed_note(job.failures.len())),
                )
                .child(detail),
        );
    }
    card.into_any_element()
}

/// A finished job's one-line note, for the completed group where a full card
/// would be five rows of history.
///
/// Not a smaller card: the difference is that this has no ring, no waveform, and
/// no controls except the two that still apply. A completed transfer has nothing
/// to pause.
pub fn finished_row<V: 'static>(job: &Job, cx: &mut Context<V>) -> AnyElement {
    let id = job.id;
    let glyph = match job.status {
        JobStatus::Failed(_) => IconName::CircleX,
        JobStatus::Cancelled => IconName::Info,
        _ => IconName::CircleCheck,
    };

    h_flex()
        .id(SharedString::from(format!("job-done-{id}")))
        .w_full()
        .px_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded(cx.theme().radius)
        .hover(|style| style.bg(cx.theme().list_hover))
        .child(
            Icon::new(glyph)
                .size(px(12.))
                .flex_none()
                .text_color(cx.theme().muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(job.title.clone()),
        )
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label::status_label(&job.status)),
        )
        // Two separate counts, because they answer different questions: `skipped`
        // is what the user told it to leave alone, `failed` is what it could not
        // do. Collapsing them would report a policy answer as a problem.
        .when(job.skipped > 0, |row| {
            row.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label::skipped_note(job.skipped)),
            )
        })
        .when(!job.failures.is_empty(), |row| {
            row.child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label::failed_note(job.failures.len())),
            )
        })
        .child(kebab(job))
        .into_any_element()
}
