//! The PIKU brand mark: a large Lucide `folder-open` line icon beside the
//! wordmark. No background shape, no gradients — the mark is the stroke alone.

use gpui::{
    App, FontWeight, IntoElement, ParentElement, Pixels, RenderOnce, Styled, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{ActiveTheme as _, Icon, IconName, h_flex};

#[derive(IntoElement)]
pub struct PikuLogo {
    icon_size: Pixels,
    wordmark: bool,
}

impl PikuLogo {
    pub fn new() -> Self {
        Self {
            icon_size: px(22.),
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
        let text_size = self.icon_size * 0.72;
        h_flex()
            .gap_2()
            .items_center()
            .child(
                Icon::new(IconName::FolderOpen)
                    .size(self.icon_size)
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
