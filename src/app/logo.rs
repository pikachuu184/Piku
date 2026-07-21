//! The PIKU brand mark: the twin-crest logomark beside the wordmark. The mark
//! is a monochrome SVG tinted to the theme foreground — no background shape,
//! no gradients.

use gpui::{
    App, FontWeight, IntoElement, ParentElement, Pixels, RenderOnce, Styled, Window, div,
    prelude::FluentBuilder as _, px, svg,
};
use gpui_component::{ActiveTheme as _, h_flex};

use crate::app::assets::PikuIcon;
use gpui_component::IconNamed as _;

/// The logomark's intrinsic aspect ratio (viewBox 50 × 40).
const MARK_ASPECT: f32 = 50.0 / 40.0;

#[derive(IntoElement)]
pub struct PikuLogo {
    /// Height of the mark. Width is derived from [`MARK_ASPECT`].
    icon_size: Pixels,
    wordmark: bool,
}

impl PikuLogo {
    pub fn new() -> Self {
        Self {
            icon_size: px(28.),
            wordmark: true,
        }
    }

    pub fn icon_size(mut self, size: Pixels) -> Self {
        self.icon_size = size;
        self
    }

    pub fn icon_only(mut self) -> Self {
        self.wordmark = false;
        self
    }
}

impl RenderOnce for PikuLogo {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let height = self.icon_size;
        let width = height * MARK_ASPECT;
        let text_size = height * 0.62;
        h_flex()
            .gap_2()
            .items_center()
            .child(
                svg()
                    .flex_none()
                    .path(PikuIcon::Logo.path())
                    .w(width)
                    .h(height)
                    .text_color(cx.theme().foreground),
            )
            .when(self.wordmark, |this| {
                this.child(
                    div()
                        .text_size(text_size)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cx.theme().foreground)
                        .child("PIKU"),
                )
            })
    }
}
