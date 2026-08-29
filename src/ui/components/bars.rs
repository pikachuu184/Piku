//! The bar-column painter both waveforms use.
//!
//! Two surfaces draw a row of vertical bars from a slice of `0.0..=1.0` samples:
//! the audio scrubber in [`media::transport`](crate::ui::media::transport), whose
//! bars are decoded peaks split at the playhead, and the throughput waveform in
//! [`transfers::waveform`](crate::ui::transfers::waveform), whose bars are a
//! rolling rate ring. The `slot` / `bar_w` / `bar_h` arithmetic is identical, and
//! it lives here so the two cannot drift into looking like different widgets.
//!
//! Colour is the caller's, per bar, which is the whole difference between them:
//! the scrubber needs played and unplayed, and the transfer waveform is one tone
//! because completion is the ring's job. That is also why this takes a closure
//! rather than a `progress: f32` — a progress split is one thing a caller can
//! express with an index, not the only thing.
//!
//! Theme rules are the caller's too. Nothing here reads `cx.theme()`; it is
//! handed the colours it paints.

use gpui::{Bounds, Hsla, Pixels, Window, fill, point, px, size};

/// Horizontal gap between bars, in pixels — split evenly either side, so a bar
/// sits centred in its slot.
const GAP: f32 = 2.0;

/// Vertical breathing room, in pixels, above and below a full-height bar.
const INSET: f32 = 8.0;

/// Shortest bar drawn, in pixels. A sample of zero still gets a mark: a gap in
/// the row would read as missing data rather than as silence or a stall.
const MIN_BAR: f32 = 2.0;

/// Where bar `index` of `count` goes, for a sample of `value`.
///
/// Pure, so the arithmetic is testable without a window — which matters more than
/// it looks, because the failure mode of getting it wrong is bars that are one
/// pixel wide or drawn outside their box, and neither errors.
///
/// `value` is clamped, so a caller that has not normalized its ring cannot paint
/// outside `bounds`.
fn bar(index: usize, count: usize, value: f32, bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let w = f32::from(bounds.size.width);
    let h = f32::from(bounds.size.height);
    let slot = (w / count.max(1) as f32).max(1.0);
    let bar_w = (slot - GAP).max(1.0);
    // `max` after the multiply rather than before: a clamped-to-zero sample still
    // gets `MIN_BAR`, and a NaN one degrades to it as well.
    let bar_h = (value.clamp(0.0, 1.0) * (h - INSET)).max(MIN_BAR);
    Bounds {
        origin: point(
            px(f32::from(bounds.origin.x) + index as f32 * slot + GAP / 2.0),
            // Centred vertically, so the row grows from the middle out. A
            // baseline-anchored bar reads as a chart; this reads as a level.
            px(f32::from(bounds.origin.y) + (h - bar_h) / 2.0),
        ),
        size: size(px(bar_w), px(bar_h)),
    }
}

/// Paint one bar per entry of `values`, oldest first, left to right.
///
/// `color_for` is called once per bar with its index. An empty `values` paints
/// nothing at all — a caller with no samples draws whatever it wants instead
/// (the scrubber draws a plain progress track), and a row of minimum-height bars
/// would be a claim that there is data.
pub fn paint_bars(
    bounds: Bounds<Pixels>,
    values: &[f32],
    color_for: impl Fn(usize) -> Hsla,
    window: &mut Window,
) {
    let count = values.len();
    for (index, value) in values.iter().enumerate() {
        window.paint_quad(fill(bar(index, count, *value, bounds), color_for(index)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn box_of(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds {
            origin: point(px(10.0), px(20.0)),
            size: size(px(w), px(h)),
        }
    }

    /// The arithmetic the two waveforms share, at the geometry the scrubber
    /// actually renders at: 64 samples in a 56 px-tall strip.
    #[test]
    fn bars_stay_inside_their_slot_and_their_box() {
        let bounds = box_of(640.0, 56.0);
        let count = 64;
        let slot = 640.0 / 64.0;

        for index in 0..count {
            let value = index as f32 / (count - 1) as f32;
            let drawn = bar(index, count, value, bounds);
            let x = f32::from(drawn.origin.x);
            let y = f32::from(drawn.origin.y);
            let (bw, bh) = (f32::from(drawn.size.width), f32::from(drawn.size.height));

            // Inside its own slot, and never overlapping the next bar.
            assert!(x >= 10.0 + index as f32 * slot, "bar {index} started early");
            assert!(
                x + bw <= 10.0 + (index + 1) as f32 * slot,
                "bar {index} ran into the next slot"
            );
            // Inside the box, vertically.
            assert!(y >= 20.0 && y + bh <= 20.0 + 56.0, "bar {index} overflowed");
            // Always visible.
            assert!(bw >= 1.0 && bh >= MIN_BAR, "bar {index} was invisible");
        }
    }

    #[test]
    fn a_silent_sample_still_gets_a_mark() {
        let drawn = bar(0, 8, 0.0, box_of(80.0, 24.0));
        assert_eq!(f32::from(drawn.size.height), MIN_BAR);
    }

    /// A ring that has not been normalized cannot paint outside the box, and a
    /// division that produced a NaN cannot paint a NaN-sized quad.
    #[test]
    fn an_out_of_range_sample_is_clamped_rather_than_trusted() {
        let bounds = box_of(80.0, 24.0);
        let full = f32::from(bar(0, 8, 1.0, bounds).size.height);
        assert_eq!(f32::from(bar(0, 8, 4.0, bounds).size.height), full);
        assert_eq!(f32::from(bar(0, 8, -1.0, bounds).size.height), MIN_BAR);
        assert_eq!(f32::from(bar(0, 8, f32::NAN, bounds).size.height), MIN_BAR);
    }

    /// More samples than pixels, which a 64-slot ring in a narrow popover hits.
    #[test]
    fn a_row_narrower_than_its_samples_still_draws_something() {
        let drawn = bar(20, 64, 0.5, box_of(30.0, 18.0));
        assert!(f32::from(drawn.size.width) >= 1.0);
        assert!(f32::from(drawn.size.height) >= MIN_BAR);
    }

    /// `count` comes from a slice length, and `paint_bars` never calls this with
    /// zero — but a divide by it would be a panic in a paint closure, which is a
    /// crashed window rather than a wrong pixel.
    #[test]
    fn a_count_of_zero_does_not_divide_by_zero() {
        let drawn = bar(0, 0, 0.5, box_of(80.0, 24.0));
        assert!(f32::from(drawn.size.width).is_finite());
        assert!(f32::from(drawn.size.width) >= 1.0);
    }
}
