//! Monochrome git status badges for file rows. Letters, not colors: the
//! staged side renders at full foreground luminance, worktree-only changes
//! at muted luminance — the brand's grayscale rule stays intact.

use gpui::{FontWeight, IntoElement, ParentElement as _, Styled as _, div, px};
use gpui_component::ActiveTheme as _;

use crate::services::git::types::{GitFileStatus, GitStatusCode};

/// Fixed-width status letter for a file row (`M`, `A`, `D`, `R`, `U`, `!`).
/// Returns `None` for clean files so callers can skip the cell entirely.
pub fn status_glyph(status: GitFileStatus, cx: &gpui::App) -> Option<impl IntoElement> {
    let code = status.primary()?;
    let (color, weight) = if code == GitStatusCode::Conflicted {
        // Conflicts must shout even in grayscale: brightest + boldest.
        (cx.theme().foreground, FontWeight::BOLD)
    } else if code == GitStatusCode::Ignored {
        // Informational only: the dimmest tier.
        (cx.theme().muted_foreground.opacity(0.6), FontWeight::NORMAL)
    } else if status.is_staged() {
        (cx.theme().foreground, FontWeight::SEMIBOLD)
    } else {
        (cx.theme().muted_foreground, FontWeight::NORMAL)
    };
    Some(
        div()
            .w(px(14.))
            .flex_none()
            .text_xs()
            .font_weight(weight)
            .text_color(color)
            .text_center()
            .child(code.glyph()),
    )
}

/// Small dot marking a directory that contains dirty entries.
pub fn dirty_dir_dot(cx: &gpui::App) -> impl IntoElement {
    div()
        .w(px(14.))
        .flex_none()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .text_center()
        .child("•")
}
