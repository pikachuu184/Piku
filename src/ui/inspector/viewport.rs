//! The shared zoom and pan transform for previews that have a viewport.
//!
//! This replaced driving a [`gpui::ScrollHandle`] as if it were a translation
//! vector. It is not one, and using it as one failed in two ways that both read
//! as "the zoom is broken":
//!
//! * **A scroll offset is clamped to the scrollable range.** Content smaller
//!   than its container has no range at all, so every offset collapsed to zero
//!   and a zoomed-out image sat in the top-left corner instead of staying
//!   centred.
//! * **Fitted and zoomed were two different layouts.** Fitted centred the image
//!   with flexbox and no offset; zoomed laid it out at the scroll origin. The
//!   anchoring arithmetic solved for a top-left origin, so the first wheel notch
//!   out of fitted snapped the image sideways by the letterbox margin.
//!
//! Here [`Viewport::pan`] is the content centre's offset from the viewport
//! centre, so centred is the zero state and both problems disappear: fitted and
//! zoomed are the same layout at different scales, and "smaller than the
//! viewport" is a clamp to zero rather than an accident.
//!
//! The engine is deliberately free of any element type. It computes numbers;
//! the preview controllers turn them into layout, which is what lets the image
//! and the PDF page share one implementation.

use gpui::{Bounds, Pixels, Point, point, px};

/// Zoom bounds, as a multiplier.
pub(super) const ZOOM_MIN: f32 = 0.1;
pub(super) const ZOOM_MAX: f32 = 8.0;

/// Largest edge, in layout pixels, that the scaled element may ask for.
///
/// The zoom *factor* is clamped to [`ZOOM_MAX`], but a factor is not a size:
/// 8× of a 6000 px photo is a 48 000 px element, which the layout and the
/// renderer both have to carry. Bounding the result rather than only the
/// multiplier keeps a large source from turning a legal zoom level into an
/// illegal element.
pub(super) const MAX_ZOOMED_EDGE: f32 = 16_384.0;

/// How the content is scaled.
#[derive(Clone, Copy, PartialEq, Default, Debug)]
pub(super) enum ImageZoom {
    /// The whole of it, inside the viewport.
    #[default]
    Fit,
    /// As wide as the viewport, however tall that makes it.
    FitWidth,
    /// One source pixel per layout pixel.
    Actual,
    Custom(f32),
}

impl ImageZoom {
    /// The scale this state resolves to. `None` before the viewport has been
    /// measured, or when the content has no pixel size of its own (SVG) — in
    /// which case there is no number to report and the caller falls back to the
    /// renderer's own `Contain`.
    pub fn scale(self, metrics: Option<Metrics>) -> Option<f32> {
        match self {
            Self::Fit => metrics.map(|m| m.fit()),
            Self::FitWidth => metrics.map(|m| m.fit_width()),
            Self::Actual => Some(1.0),
            Self::Custom(scale) => Some(scale),
        }
    }

    /// Multiply the current scale, resolving a fitted mode against the measured
    /// viewport first so the step continues from what is on screen rather than
    /// jumping to 100 %.
    pub fn stepped(self, factor: f32, metrics: Option<Metrics>) -> Self {
        let base = self.scale(metrics).unwrap_or(1.0);
        Self::Custom((base * factor).clamp(ZOOM_MIN, ZOOM_MAX))
    }

    pub fn is_fit(self) -> bool {
        matches!(self, Self::Fit)
    }
}

/// Everything derived from a measured viewport box and the content's size.
///
/// Built once per render and threaded through, so the readout, the layout and
/// the anchoring arithmetic cannot disagree about what "100 %" means.
/// Constructing it at all proves the box and the content are non-degenerate,
/// which is why the rest of this module can divide freely.
#[derive(Clone, Copy, Debug)]
pub(super) struct Metrics {
    bounds: Bounds<Pixels>,
    /// The content's size in source pixels: what is actually laid out.
    displayed: (u32, u32),
}

impl Metrics {
    pub fn new(bounds: Bounds<Pixels>, dimensions: (u32, u32)) -> Option<Self> {
        let displayed = dimensions;
        if displayed.0 == 0 || displayed.1 == 0 {
            return None;
        }
        if f32::from(bounds.size.width) <= 0.0 || f32::from(bounds.size.height) <= 0.0 {
            return None;
        }
        Some(Self { bounds, displayed })
    }

    pub fn displayed(&self) -> (u32, u32) {
        self.displayed
    }

    pub fn center(&self) -> Point<Pixels> {
        self.bounds.center()
    }

    fn view(&self) -> (f32, f32) {
        (
            f32::from(self.bounds.size.width),
            f32::from(self.bounds.size.height),
        )
    }

    fn content(&self) -> (f32, f32) {
        (self.displayed.0 as f32, self.displayed.1 as f32)
    }

    /// The scale that shows the whole of the content inside the viewport.
    ///
    /// `.min(1.0)` — the same as Zed's `compute_fit_to_view_zoom`, and the same
    /// as what is already on screen. The predecessor of this function omitted
    /// it and claimed in its doc comment that fitted mode upscales small
    /// images. It does not: fitted rendered through `ObjectFit::Contain` inside
    /// an element bounded by `max_w_full`/`max_h_full`, which caps a small
    /// image at its natural size. So the number was wrong, not the picture —
    /// a 32 px icon reported "400 %" while drawing at 1:1. Blowing an icon up
    /// to fill the panel is also not what a viewer should do on "fit".
    pub fn fit(&self) -> f32 {
        let (vw, vh) = self.view();
        let (cw, ch) = self.content();
        (vw / cw).min(vh / ch).min(1.0)
    }

    /// The scale that makes the content exactly as wide as the viewport. It may
    /// well be taller than the viewport afterwards; that is the point.
    pub fn fit_width(&self) -> f32 {
        let (vw, _) = self.view();
        let (cw, _) = self.content();
        vw / cw
    }

    /// The scale actually used to lay the content out, after [`MAX_ZOOMED_EDGE`].
    pub fn effective(&self, scale: f32) -> f32 {
        let longest = self.displayed.0.max(self.displayed.1) as f32;
        if longest <= 0.0 {
            return scale;
        }
        scale.min(MAX_ZOOMED_EDGE / longest)
    }

    /// Content size in layout pixels at `scale`.
    pub fn scaled(&self, scale: f32) -> (f32, f32) {
        let (cw, ch) = self.content();
        (cw * scale, ch * scale)
    }
}

/// The transform state for one zoomable preview.
pub(super) struct Viewport {
    pub zoom: ImageZoom,
    /// The content centre's offset from the viewport centre, in layout pixels.
    /// Zero is centred, which is the resting state at every scale.
    pub pan: Point<Pixels>,
    /// Where the current drag was last seen. [`gpui::MouseMoveEvent`] carries
    /// no delta of its own, so the difference has to be kept somewhere.
    pub drag_from: Option<Point<Pixels>>,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            zoom: ImageZoom::default(),
            pan: point(px(0.), px(0.)),
            drag_from: None,
        }
    }
}

impl Viewport {
    pub fn scale(&self, metrics: Option<Metrics>) -> Option<f32> {
        self.zoom.scale(metrics)
    }

    /// The scale to lay out with, after the element-size bound.
    pub fn effective_scale(&self, metrics: Option<Metrics>) -> Option<f32> {
        let scale = self.scale(metrics)?;
        Some(match metrics {
            Some(metrics) => metrics.effective(scale),
            None => scale,
        })
    }

    /// Whether this is the untouched resting state.
    ///
    /// Load-bearing: only in this state may the caller render through the
    /// renderer's own `ObjectFit::Contain`, which needs no measurement and so
    /// is correct on the very first frame and stays correct while the dock is
    /// being dragged. A computed scale would be one frame behind for the whole
    /// of that drag.
    pub fn is_untransformed(&self) -> bool {
        self.zoom.is_fit() && f32::from(self.pan.x) == 0.0 && f32::from(self.pan.y) == 0.0
    }

    /// Whether the content is displaced from the centre of its box.
    ///
    /// `allow`, not `expect`: the tests below are the only caller. The render
    /// path asks the stronger [`Self::is_untransformed`], which also covers the
    /// zoom, because that is the one state it may draw through `Contain`.
    #[allow(dead_code, reason = "test vocabulary")]
    pub fn is_panned(&self) -> bool {
        f32::from(self.pan.x) != 0.0 || f32::from(self.pan.y) != 0.0
    }

    /// Switch zoom mode, anchoring on the viewport centre.
    ///
    /// Fit means "all of it", so it also discards the pan — otherwise fitting
    /// could leave the content centred on nothing.
    pub fn set_zoom(&mut self, zoom: ImageZoom, metrics: Option<Metrics>) {
        if zoom.is_fit() {
            self.zoom = zoom;
            self.pan = point(px(0.), px(0.));
            return;
        }
        let before = self.effective_scale(metrics);
        self.zoom = zoom;
        let after = self.effective_scale(metrics);
        if let (Some(before), Some(after)) = (before, after)
            && before > 0.0
        {
            // Anchored at the centre, the cursor term drops out and the
            // correction is just the ratio (see `zoom_about`).
            let ratio = after / before;
            self.pan = point(
                px(f32::from(self.pan.x) * ratio),
                px(f32::from(self.pan.y) * ratio),
            );
        }
        self.clamp_pan(metrics);
    }

    /// Step the zoom while keeping the content point under `cursor` in place.
    ///
    /// Without this the content slides out from under the pointer as it grows.
    /// Working in coordinates relative to the viewport centre, a content-space
    /// offset `q` from the content centre lands on screen at `pan + q · scale`.
    /// Solving for the pan that puts the same `q` back under the cursor:
    ///
    /// ```text
    /// q   = (cursor − pan₀) / scale₀
    /// pan₁ = cursor − q · scale₁
    ///      = cursor · (1 − r) + pan₀ · r,   where r = scale₁ / scale₀
    /// ```
    pub fn zoom_about(&mut self, cursor: Point<Pixels>, factor: f32, metrics: Option<Metrics>) {
        let Some(metrics) = metrics else {
            // Unmeasured or unsized content: there is a zoom level but no
            // geometry to anchor it against.
            self.zoom = self.zoom.stepped(factor, None);
            return;
        };
        let before = self.effective_scale(Some(metrics)).unwrap_or(1.0);
        self.zoom = self.zoom.stepped(factor, Some(metrics));
        let after = self.effective_scale(Some(metrics)).unwrap_or(1.0);

        if before > 0.0 && after > 0.0 && before != after {
            let ratio = after / before;
            let relative = cursor - metrics.center();
            self.pan = point(
                px(f32::from(relative.x) * (1.0 - ratio) + f32::from(self.pan.x) * ratio),
                px(f32::from(relative.y) * (1.0 - ratio) + f32::from(self.pan.y) * ratio),
            );
        }
        self.clamp_pan(Some(metrics));
    }

    /// Step the zoom about the viewport centre. What the toolbar buttons and
    /// the keyboard use; the predecessor of this changed the scale and left the
    /// pan alone, so button zoom drifted while wheel zoom did not.
    pub fn zoom_by(&mut self, factor: f32, metrics: Option<Metrics>) {
        match metrics {
            Some(metrics) => self.zoom_about(metrics.center(), factor, Some(metrics)),
            None => self.zoom = self.zoom.stepped(factor, None),
        }
    }

    pub fn pan_by(&mut self, delta: Point<Pixels>, metrics: Option<Metrics>) {
        self.pan = point(self.pan.x + delta.x, self.pan.y + delta.y);
        self.clamp_pan(metrics);
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Keep the content inside the viewport.
    ///
    /// On an axis where the scaled content is no larger than the viewport there
    /// is nothing to pan to, so it is pinned centred — this is the case the
    /// scroll handle could not express, and the reason zoomed-out content used
    /// to end up in the corner. On a larger axis the pan is bounded so the
    /// content's edge cannot come inside the viewport's edge, which stops the
    /// content being dragged off screen entirely.
    fn clamp_pan(&mut self, metrics: Option<Metrics>) {
        let Some(metrics) = metrics else { return };
        let Some(scale) = self.effective_scale(Some(metrics)) else {
            return;
        };
        let (content_w, content_h) = metrics.scaled(scale);
        let (view_w, view_h) = metrics.view();
        self.pan = point(
            px(clamp_axis(f32::from(self.pan.x), content_w, view_w)),
            px(clamp_axis(f32::from(self.pan.y), content_h, view_h)),
        );
    }
}

fn clamp_axis(pan: f32, content: f32, view: f32) -> f32 {
    if content <= view {
        return 0.0;
    }
    let limit = (content - view) / 2.0;
    pan.clamp(-limit, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Bounds, size};

    fn metrics(view: (f32, f32), content: (u32, u32)) -> Metrics {
        let bounds = Bounds {
            origin: point(px(100.), px(50.)),
            size: size(px(view.0), px(view.1)),
        };
        Metrics::new(bounds, content).expect("non-degenerate metrics")
    }

    /// Where a content-space offset from the content centre lands on screen.
    fn screen_of(q: (f32, f32), viewport: &Viewport, m: Metrics) -> (f32, f32) {
        let scale = viewport.effective_scale(Some(m)).unwrap();
        let center = m.center();
        (
            f32::from(center.x) + f32::from(viewport.pan.x) + q.0 * scale,
            f32::from(center.y) + f32::from(viewport.pan.y) + q.1 * scale,
        )
    }

    /// The defect this module exists for: the point under the cursor must not
    /// move when the scale changes.
    #[test]
    fn zooming_keeps_the_point_under_the_cursor() {
        // Content much larger than the viewport, so the clamp never engages.
        let m = metrics((400.0, 300.0), (4000, 3000));
        let mut viewport = Viewport {
            zoom: ImageZoom::Custom(0.5),
            ..Default::default()
        };

        // A point well off the content centre, and the cursor currently over it.
        let q = (-300.0, 220.0);
        let (cursor_x, cursor_y) = screen_of(q, &viewport, m);
        let cursor = point(px(cursor_x), px(cursor_y));

        viewport.zoom_about(cursor, 1.25, Some(m));

        let (after_x, after_y) = screen_of(q, &viewport, m);
        assert!(
            (after_x - cursor_x).abs() < 0.01 && (after_y - cursor_y).abs() < 0.01,
            "point drifted from ({cursor_x}, {cursor_y}) to ({after_x}, {after_y})"
        );
    }

    /// The first notch out of fitted mode used to snap the image sideways,
    /// because fitted and zoomed were laid out against different origins.
    #[test]
    fn the_first_step_out_of_fit_does_not_snap() {
        let m = metrics((400.0, 300.0), (4000, 3000));
        let mut viewport = Viewport::default();
        let q = (-1000.0, 500.0);
        let (cursor_x, cursor_y) = screen_of(q, &viewport, m);

        viewport.zoom_about(point(px(cursor_x), px(cursor_y)), 1.25, Some(m));

        let (after_x, after_y) = screen_of(q, &viewport, m);
        assert!(
            (after_x - cursor_x).abs() < 0.01 && (after_y - cursor_y).abs() < 0.01,
            "snapped from ({cursor_x}, {cursor_y}) to ({after_x}, {after_y})"
        );
    }

    /// Content smaller than the viewport has nowhere to pan to, so it stays
    /// centred. A scroll offset clamped this to the top-left corner instead.
    #[test]
    fn content_smaller_than_the_viewport_stays_centred() {
        let m = metrics((800.0, 600.0), (400, 300));
        let mut viewport = Viewport {
            zoom: ImageZoom::Custom(0.5),
            ..Default::default()
        };

        viewport.pan_by(point(px(250.), px(-180.)), Some(m));
        assert_eq!(f32::from(viewport.pan.x), 0.0);
        assert_eq!(f32::from(viewport.pan.y), 0.0);

        // And zooming out toward it re-centres rather than cornering.
        viewport.zoom_about(point(px(120.), px(60.)), 0.8, Some(m));
        assert_eq!(f32::from(viewport.pan.x), 0.0);
        assert_eq!(f32::from(viewport.pan.y), 0.0);
    }

    #[test]
    fn panning_cannot_push_the_content_off_screen() {
        let m = metrics((400.0, 300.0), (800, 600));
        let mut viewport = Viewport {
            zoom: ImageZoom::Actual,
            ..Default::default()
        };

        viewport.pan_by(point(px(10_000.), px(10_000.)), Some(m));
        // Content 800×600 in a 400×300 box: half the overhang each way.
        assert_eq!(f32::from(viewport.pan.x), 200.0);
        assert_eq!(f32::from(viewport.pan.y), 150.0);
    }

    #[test]
    fn returning_to_fit_discards_the_pan() {
        let m = metrics((400.0, 300.0), (4000, 3000));
        let mut viewport = Viewport::default();

        viewport.zoom_about(point(px(120.), px(70.)), 1.25, Some(m));
        viewport.pan_by(point(px(60.), px(40.)), Some(m));
        assert!(
            viewport.is_panned(),
            "test needs a non-zero pan to be useful"
        );

        viewport.set_zoom(ImageZoom::Fit, Some(m));
        assert!(viewport.is_untransformed());
    }

    #[test]
    fn fit_never_upscales_but_fit_width_may() {
        let m = metrics((800.0, 600.0), (100, 50));
        assert_eq!(m.fit(), 1.0, "fit must not blow a small image up");
        assert_eq!(m.fit_width(), 8.0, "fit-width is explicitly asked for");
    }

    #[test]
    fn the_element_bound_survives_a_legal_zoom_factor() {
        // 8× of a 6000 px edge is 48 000 px; the element bound cuts it back.
        let m = metrics((400.0, 300.0), (6000, 4000));
        let effective = m.effective(ZOOM_MAX);
        assert!(effective < ZOOM_MAX);
        assert!((effective * 6000.0 - MAX_ZOOMED_EDGE).abs() < 0.01);
    }

    #[test]
    fn zoom_stays_within_bounds() {
        let m = metrics((400.0, 300.0), (4000, 3000));
        let mut viewport = Viewport::default();
        for _ in 0..64 {
            viewport.zoom_by(1.25, Some(m));
        }
        assert_eq!(viewport.zoom, ImageZoom::Custom(ZOOM_MAX));
        for _ in 0..128 {
            viewport.zoom_by(0.8, Some(m));
        }
        assert_eq!(viewport.zoom, ImageZoom::Custom(ZOOM_MIN));
    }

    #[test]
    fn degenerate_geometry_yields_no_metrics() {
        let box_ = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(400.), px(300.)),
        };
        assert!(Metrics::new(box_, (0, 100)).is_none());
        assert!(Metrics::new(box_, (100, 0)).is_none());

        let flat = Bounds {
            origin: point(px(0.), px(0.)),
            size: size(px(0.), px(300.)),
        };
        assert!(Metrics::new(flat, (100, 100)).is_none());
    }

    /// Unmeasured content still zooms; it just cannot anchor.
    #[test]
    fn zooming_without_metrics_still_changes_the_level() {
        let mut viewport = Viewport::default();
        viewport.zoom_about(point(px(10.), px(10.)), 1.25, None);
        assert_eq!(viewport.zoom, ImageZoom::Custom(1.25));
        assert_eq!(f32::from(viewport.pan.x), 0.0);
    }
}
