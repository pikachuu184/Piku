//! Drive rows with segmented capacity bars: one solid color per file
//! category (from the background drive scan), neutral free space, and a
//! single red bar when the drive is nearly full. Hovering a segment names
//! the category and the bytes it occupies.

use gpui::{
    App, Div, InteractiveElement as _, ParentElement, StatefulInteractiveElement as _, Styled,
    div, px, relative,
};
use gpui_component::{ActiveTheme as _, h_flex, tooltip::Tooltip, v_flex};

use crate::core::file_type::FileCategory;
use crate::core::format::format_size;
use crate::services::drive_scan::SCAN_CATEGORIES;
use crate::services::fs_service::DriveInfo;
use crate::state::PikuState;
use crate::theme::category_colors::{NEAR_FULL_FRACTION, category_color, near_full_color};

/// Bar height — tall enough that the color segments read clearly.
const BAR_HEIGHT: f32 = 6.;
/// Colored categories below this share of the disk fold into "Other".
const MIN_SEGMENT_FRACTION: f64 = 0.01;

pub fn drive_details(ix: usize, drive: &DriveInfo, cx: &App) -> Div {
    let used = drive.total.saturating_sub(drive.available);
    let used_fraction = if drive.total > 0 {
        (used as f64 / drive.total as f64) as f32
    } else {
        0.0
    };

    v_flex()
        .flex_1()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .justify_between()
                .items_center()
                .child(
                    div()
                        .text_sm()
                        .truncate()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(drive.name.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} free", format_size(drive.available))),
                ),
        )
        .child(usage_bar(ix, drive, used, used_fraction, cx))
}

fn usage_bar(ix: usize, drive: &DriveInfo, used: u64, used_fraction: f32, cx: &App) -> Div {
    let track = h_flex()
        .w_full()
        .h(px(BAR_HEIGHT))
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .bg(cx.theme().border);

    // Nearly full: capacity is the story — one solid red bar.
    if used_fraction >= NEAR_FULL_FRACTION {
        let text = format!(
            "Used — {} ({:.0}%)",
            format_size(used),
            f64::from(used_fraction) * 100.0
        );
        return track
            .child(segment(("drive-used", ix), used_fraction, near_full_color(), text))
            .child(free_segment(ix, drive.available));
    }

    let stats = PikuState::global(cx)
        .drive_stats
        .read(cx)
        .get(&drive.mount)
        .cloned();

    // No scan data yet: today's monochrome fill, now with a hover summary.
    let Some(stats) = stats else {
        let text = format!(
            "Used — {} ({:.0}%)",
            format_size(used),
            f64::from(used_fraction) * 100.0
        );
        return track
            .child(segment(
                ("drive-used", ix),
                used_fraction,
                cx.theme().foreground,
                text,
            ))
            .child(free_segment(ix, drive.available));
    };

    let total = drive.total.max(1) as f64;

    // Colored categories large enough to read; the rest folds into Other,
    // along with everything the scan could not attribute (system files,
    // inaccessible directories, depth-capped subtrees).
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

    // Hard links and sparse files can make scanned bytes exceed the OS used
    // figure — scale widths so the bar never overstates usage.
    let sum_bytes: u64 = segments.iter().map(|(_, b)| *b).sum();
    let scale = if sum_bytes > used && sum_bytes > 0 {
        used as f64 / sum_bytes as f64
    } else {
        1.0
    };

    let mut track = track;
    for (slot, (category, bytes)) in segments.into_iter().enumerate() {
        let fraction = ((bytes as f64 * scale) / total) as f32;
        let text = format!("{} — {}", category.label(), format_size(bytes));
        track = track.child(segment(
            ("drive-seg", ix * 16 + slot),
            fraction,
            category_color(category),
            text,
        ));
    }
    track.child(free_segment(ix, drive.available))
}

fn segment(
    id: (&'static str, usize),
    fraction: f32,
    color: gpui::Hsla,
    text: String,
) -> impl gpui::IntoElement {
    div()
        .id(id)
        .h_full()
        .flex_none()
        .w(relative(fraction.clamp(0.0, 1.0)))
        .bg(color)
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
}

fn free_segment(ix: usize, available: u64) -> impl gpui::IntoElement {
    let text = format!("Free — {}", format_size(available));
    div()
        .id(("drive-free", ix))
        .h_full()
        .flex_1()
        .tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
}
