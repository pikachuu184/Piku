//! Drive tiles with circular capacity rings: a thick donut whose colored
//! segments are the per-category usage from the background drive scan
//! (same palette as before — the one sanctioned hue exception), neutral
//! free space on the ring track, and the used percentage centered in the hole.
//! Tiles flow two per row and wrap with the drive count.

use gpui::prelude::FluentBuilder as _;
use gpui::{App, Div, Hsla, IntoElement, ParentElement, Styled, div, px, relative};
use gpui_component::{ActiveTheme as _, chart::PieChart, menu::ContextMenuExt as _, v_flex};

use crate::app::actions::SetDriveDisplay;
use crate::core::file_type::FileCategory;
use crate::core::format::format_size;
use crate::services::drive_scan::SCAN_CATEGORIES;
use crate::services::fs_service::DriveInfo;
use crate::state::PikuState;
use crate::state::nav_model::DriveDisplayMode;
use crate::theme::category_colors::{NEAR_FULL_FRACTION, category_color, near_full_color};

/// Ring geometry: hole ≈ 60% of the outer diameter, leaving a ~12px stroke
/// so every color segment reads clearly at the smaller tile size.
const RING_SIZE: f32 = 64.;
const OUTER_RADIUS: f32 = 30.;
const INNER_RADIUS: f32 = 18.;
/// Small angular gap between segments so adjacent colors never blur.
const PAD_ANGLE: f32 = 0.03;
/// Colored categories below this share of the disk fold into "Other".
const MIN_SEGMENT_FRACTION: f64 = 0.01;

/// One slice of the capacity ring (already scaled; free space included).
#[derive(Clone)]
struct RingSlice {
    bytes: f32,
    color: Hsla,
}

/// The colored ring segments for one drive, mirroring the old bar's rules:
/// nearly-full drives show one red "used" arc; drives without scan data show
/// a single monochrome "used" arc; otherwise per-category segments with
/// small ones folded into Other. Free space is always the last, track-colored
/// slice so the ring reads as a complete circle.
fn ring_slices(drive: &DriveInfo, cx: &App) -> Vec<RingSlice> {
    let used = drive.total.saturating_sub(drive.available);
    let used_fraction = if drive.total > 0 {
        used as f64 / drive.total as f64
    } else {
        0.0
    };
    let free_slice = RingSlice {
        bytes: drive.available.max(1) as f32,
        color: cx.theme().border,
    };

    // Nearly full: capacity is the story — one solid red arc.
    if used_fraction >= f64::from(NEAR_FULL_FRACTION) {
        return vec![
            RingSlice {
                bytes: used as f32,
                color: near_full_color(),
            },
            free_slice,
        ];
    }

    let stats = PikuState::global(cx)
        .drive_stats
        .read(cx)
        .get(&drive.mount)
        .cloned();

    // No scan data yet: monochrome used arc.
    let Some(stats) = stats else {
        return vec![
            RingSlice {
                bytes: used.max(1) as f32,
                color: cx.theme().foreground,
            },
            free_slice,
        ];
    };

    let total = drive.total.max(1) as f64;
    let mut other_bytes = stats.bytes_for(FileCategory::Other);
    let mut colored: Vec<(FileCategory, u64)> = SCAN_CATEGORIES
        .iter()
        .filter(|c| **c != FileCategory::Other)
        .map(|c| (*c, stats.bytes_for(*c)))
        .filter(|(_, b)| *b > 0)
        .collect();
    colored.retain(|(_, b)| {
        if (*b as f64 / total) < MIN_SEGMENT_FRACTION {
            other_bytes += *b;
            false
        } else {
            true
        }
    });
    colored.sort_by_key(|(_, b)| std::cmp::Reverse(*b));

    let scanned: u64 = colored.iter().map(|(_, b)| *b).sum::<u64>() + other_bytes;
    if scanned < used {
        other_bytes += used - scanned;
    }
    let mut segments = colored;
    if other_bytes > 0 {
        segments.push((FileCategory::Other, other_bytes));
    }

    // Hard links / sparse files can make scanned bytes exceed the OS used
    // figure — scale so the ring never overstates usage.
    let sum_bytes: u64 = segments.iter().map(|(_, b)| *b).sum();
    let scale = if sum_bytes > used && sum_bytes > 0 {
        used as f64 / sum_bytes as f64
    } else {
        1.0
    };

    let mut slices: Vec<RingSlice> = segments
        .into_iter()
        .map(|(category, bytes)| RingSlice {
            bytes: (bytes as f64 * scale) as f32,
            color: category_color(category),
        })
        .collect();
    slices.push(free_slice);
    slices
}

/// Tooltip body: per-category byte breakdown (the ring itself cannot host
/// per-segment tooltips — it is one painted canvas).
pub fn drive_summary(drive: &DriveInfo, cx: &App) -> String {
    let used = drive.total.saturating_sub(drive.available);
    let percent = if drive.total > 0 {
        used as f64 / drive.total as f64 * 100.0
    } else {
        0.0
    };
    let mut text = format!(
        "{} used of {} — {} free ({percent:.0}%)",
        format_size(used),
        format_size(drive.total),
        format_size(drive.available),
    );
    if let Some(stats) = PikuState::global(cx).drive_stats.read(cx).get(&drive.mount) {
        for category in SCAN_CATEGORIES {
            let bytes = stats.bytes_for(category);
            if bytes > 0 {
                text.push_str(&format!("\n{} — {}", category.label(), format_size(bytes)));
            }
        }
    }
    text
}

fn drive_name(drive: &DriveInfo, is_bar: bool, cx: &App) -> impl IntoElement {
    let mount = drive.mount.clone();
    let label = div()
        .when(is_bar, |label| label.w_full().text_left())
        .when(!is_bar, |label| label.max_w(px(96.)).text_center())
        .min_w_0()
        .text_xs()
        .truncate()
        .text_color(cx.theme().sidebar_foreground)
        .child(drive.name.clone());
    label.context_menu(move |menu, _, cx| {
        let current = PikuState::global(cx).nav.read(cx).drive_display(&mount);
        menu.menu_with_check(
            "Ring",
            current == DriveDisplayMode::Ring,
            Box::new(SetDriveDisplay(mount.clone(), DriveDisplayMode::Ring)),
        )
        .menu_with_check(
            "Bar",
            current == DriveDisplayMode::Bar,
            Box::new(SetDriveDisplay(mount.clone(), DriveDisplayMode::Bar)),
        )
    })
}

fn capacity_bar(slices: &[RingSlice], cx: &App) -> Div {
    let total: f32 = slices.iter().map(|slice| slice.bytes).sum();
    div()
        .flex()
        .w_full()
        .h(px(8.))
        .rounded_full()
        .overflow_hidden()
        .bg(cx.theme().border)
        .children(slices.iter().enumerate().map(|(ix, slice)| {
            let fraction = if total > 0. {
                (slice.bytes / total).clamp(0., 1.)
            } else {
                0.
            };
            div()
                .h_full()
                .w(relative(fraction))
                .bg(slice.color)
                .when(ix == 0, |segment| segment.rounded_l_full())
                .when(ix + 1 == slices.len(), |segment| segment.rounded_r_full())
        }))
}

/// One drive tile. The selected display changes only presentation; both views
/// use the same capacity slices and the caller owns navigation and tooltip.
pub fn drive_tile(drive: &DriveInfo, mode: DriveDisplayMode, cx: &App) -> Div {
    let slices = ring_slices(drive, cx);
    let used = drive.total.saturating_sub(drive.available);
    let used_percent = if drive.total > 0 {
        used as f64 / drive.total as f64 * 100.0
    } else {
        0.0
    };

    let ring = div()
        .w(px(RING_SIZE))
        .h(px(RING_SIZE))
        .relative()
        .child(
            PieChart::new(slices.clone())
                .value(|slice: &RingSlice| slice.bytes)
                .color(|slice: &RingSlice| slice.color)
                .inner_radius(INNER_RADIUS)
                .outer_radius(OUTER_RADIUS)
                .pad_angle(PAD_ANGLE),
        )
        .child(
            // Used percentage centered in the ring's hole.
            div()
                .absolute()
                .inset_0()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .max_w(px(36.))
                        .truncate()
                        .text_sm()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(format!("{used_percent:.0}%")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("used"),
                ),
        );

    match mode {
        DriveDisplayMode::Ring => v_flex()
            .w(px(76.))
            .flex_none()
            .items_center()
            .gap_1()
            .child(ring)
            .child(drive_name(drive, false, cx)),
        DriveDisplayMode::Bar => v_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .px_2()
            .py_1()
            .child(drive_name(drive, true, cx))
            .child(capacity_bar(&slices, cx))
            .child(
                gpui_component::h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .justify_between()
                    .gap_1()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(div().flex_1().min_w_0().truncate().child(format!(
                        "{} of {} used",
                        format_size(used),
                        format_size(drive.total)
                    )))
                    .child(
                        div()
                            .flex_none()
                            .child(format!("{} free", format_size(drive.available))),
                    ),
            ),
    }
}
