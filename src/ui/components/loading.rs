//! Monochrome loading primitives: a muted spinner and pulsing skeleton rows.
//! Every async region in PIKU signals progress through one of these so the
//! loading language stays consistent across panels.

use gpui::{App, IntoElement, ParentElement, Styled, div, px, relative};
use gpui_component::{ActiveTheme as _, Sizable as _, Size, skeleton::Skeleton, spinner::Spinner};

/// The PIKU spinner: the stock rotating loader in the muted foreground color
/// so it never outshines content.
pub fn piku_spinner(size: Size, cx: &App) -> impl IntoElement {
    Spinner::new()
        .with_size(size)
        .color(cx.theme().muted_foreground)
}

/// Centered spinner with an optional muted caption, for full-region loading.
#[allow(dead_code)]
pub fn loading_overlay(label: Option<&'static str>, cx: &App) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .child(piku_spinner(Size::Medium, cx))
        .children(label.map(|label| {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(label)
        }))
}

/// Pulsing skeleton placeholder rows shaped like list rows. Widths alternate
/// for an organic look; `row_h` matches the row height they stand in for.
pub fn skeleton_rows(count: usize, row_h: f32, cx: &App) -> impl IntoElement {
    // Cycle of relative widths so the block does not read as a solid slab.
    const WIDTHS: [f32; 4] = [1.0, 0.72, 0.88, 0.55];
    let radius = cx.theme().radius;
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap_2()
        .children((0..count).map(move |ix| {
            let width = WIDTHS[ix % WIDTHS.len()];
            div()
                .h(px(row_h))
                .w(relative(width))
                .flex()
                .items_center()
                .gap_3()
                .child(
                    Skeleton::new()
                        .size(px(row_h * 0.55))
                        .flex_none()
                        .rounded(radius),
                )
                .child(
                    Skeleton::new()
                        .h(px(row_h * 0.4))
                        .w(relative(1.))
                        .rounded(radius),
                )
        }))
}
