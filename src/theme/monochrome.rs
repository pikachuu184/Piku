//! PIKU monochromatic minimalism theme.
//!
//! Pure solid grays and blacks — no gradients, no hues. Depth and hierarchy
//! come exclusively from luminance steps; white is reserved for emphasis.

use gpui::{App, Hsla, px, rgb};
use gpui_component::{Theme, ThemeMode};

pub const BG_BASE: u32 = 0x0A0A0A;
pub const BG_PANEL: u32 = 0x141414;
pub const BG_ELEVATED: u32 = 0x1A1A1A;
pub const BG_HOVER: u32 = 0x1F1F1F;
pub const BG_ACTIVE: u32 = 0x262626;
pub const BORDER: u32 = 0x262626;
pub const BORDER_STRONG: u32 = 0x333333;
pub const TEXT_PRIMARY: u32 = 0xEDEDED;
pub const TEXT_SECONDARY: u32 = 0xA3A3A3;
pub const TEXT_MUTED: u32 = 0x737373;
pub const ACCENT: u32 = 0xFFFFFF;

/// The strict corner radius used across the whole application.
pub const RADIUS: f32 = 6.0;

/// Checkerboard grays for the image-preview transparency backdrop.
pub const CHECKER_A: u32 = 0x1A1A1A;
pub const CHECKER_B: u32 = 0x212121;

pub fn solid(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn alpha(hex: u32, a: f32) -> Hsla {
    let mut color: Hsla = rgb(hex).into();
    color.a = a;
    color
}

/// Apply the PIKU monochrome palette on top of the built-in dark theme.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);

    let theme = Theme::global_mut(cx);
    theme.radius = px(RADIUS);
    theme.radius_lg = px(RADIUS);
    theme.tile_radius = px(RADIUS);
    theme.shadow = false;
    theme.tile_shadow = false;
    theme.highlight_theme = monochrome_highlight_theme();

    let c = &mut theme.colors;

    c.background = solid(BG_BASE);
    c.foreground = solid(TEXT_PRIMARY);
    c.border = solid(BORDER);
    c.input = solid(BORDER);
    c.ring = solid(0x525252);
    c.caret = solid(TEXT_PRIMARY);
    c.overlay = alpha(0x000000, 0.6);
    c.window_border = solid(BORDER);
    c.selection = alpha(ACCENT, 0.15);
    c.drag_border = solid(ACCENT);
    c.drop_target = alpha(ACCENT, 0.08);

    c.muted = solid(BG_ELEVATED);
    c.muted_foreground = solid(TEXT_MUTED);
    c.accent = solid(BG_HOVER);
    c.accent_foreground = solid(TEXT_PRIMARY);

    c.primary = solid(ACCENT);
    c.primary_foreground = solid(BG_BASE);
    c.primary_hover = solid(0xE5E5E5);
    c.primary_active = solid(0xD4D4D4);

    c.secondary = solid(BG_ELEVATED);
    c.secondary_foreground = solid(TEXT_PRIMARY);
    c.secondary_hover = solid(BG_HOVER);
    c.secondary_active = solid(BG_ACTIVE);

    c.danger = solid(TEXT_PRIMARY);
    c.danger_foreground = solid(BG_BASE);
    c.danger_hover = solid(ACCENT);
    c.danger_active = solid(0xD4D4D4);
    c.info = solid(TEXT_PRIMARY);
    c.info_foreground = solid(BG_BASE);
    c.info_hover = solid(ACCENT);
    c.info_active = solid(0xD4D4D4);
    c.success = solid(TEXT_PRIMARY);
    c.success_foreground = solid(BG_BASE);
    c.success_hover = solid(ACCENT);
    c.success_active = solid(0xD4D4D4);
    c.warning = solid(TEXT_SECONDARY);
    c.warning_foreground = solid(BG_BASE);
    c.warning_hover = solid(0xB5B5B5);
    c.warning_active = solid(0x8C8C8C);

    c.button = solid(BG_ELEVATED);
    c.button_foreground = solid(TEXT_PRIMARY);
    c.button_hover = solid(BG_HOVER);
    c.button_active = solid(BG_ACTIVE);
    c.button_primary = solid(ACCENT);
    c.button_primary_foreground = solid(BG_BASE);
    c.button_primary_hover = solid(0xE5E5E5);
    c.button_primary_active = solid(0xD4D4D4);
    c.button_secondary = solid(BG_ELEVATED);
    c.button_secondary_foreground = solid(TEXT_PRIMARY);
    c.button_secondary_hover = solid(BG_HOVER);
    c.button_secondary_active = solid(BG_ACTIVE);
    c.button_danger = solid(TEXT_PRIMARY);
    c.button_danger_foreground = solid(BG_BASE);
    c.button_danger_hover = solid(ACCENT);
    c.button_danger_active = solid(0xD4D4D4);
    c.button_info = solid(BG_ELEVATED);
    c.button_info_foreground = solid(TEXT_PRIMARY);
    c.button_info_hover = solid(BG_HOVER);
    c.button_info_active = solid(BG_ACTIVE);
    c.button_success = solid(BG_ELEVATED);
    c.button_success_foreground = solid(TEXT_PRIMARY);
    c.button_success_hover = solid(BG_HOVER);
    c.button_success_active = solid(BG_ACTIVE);
    c.button_warning = solid(BG_ELEVATED);
    c.button_warning_foreground = solid(TEXT_PRIMARY);
    c.button_warning_hover = solid(BG_HOVER);
    c.button_warning_active = solid(BG_ACTIVE);

    c.link = solid(TEXT_PRIMARY);
    c.link_hover = solid(ACCENT);
    c.link_active = solid(ACCENT);

    c.list = solid(BG_BASE);
    c.list_even = solid(BG_BASE);
    c.list_head = solid(BG_PANEL);
    c.list_hover = solid(BG_HOVER);
    c.list_active = solid(BG_ACTIVE);
    c.list_active_border = solid(BORDER_STRONG);

    c.table = solid(BG_BASE);
    c.table_even = solid(0x101010);
    c.table_head = solid(BG_PANEL);
    c.table_head_foreground = solid(TEXT_SECONDARY);
    c.table_foot = solid(BG_PANEL);
    c.table_foot_foreground = solid(TEXT_SECONDARY);
    c.table_hover = solid(BG_HOVER);
    c.table_active = solid(BG_ACTIVE);
    c.table_active_border = solid(BORDER_STRONG);
    c.table_row_border = solid(BG_ELEVATED);

    c.sidebar = solid(BG_PANEL);
    c.sidebar_foreground = solid(TEXT_SECONDARY);
    c.sidebar_border = solid(BORDER);
    c.sidebar_accent = solid(BG_HOVER);
    c.sidebar_accent_foreground = solid(TEXT_PRIMARY);
    c.sidebar_primary = solid(ACCENT);
    c.sidebar_primary_foreground = solid(BG_BASE);

    c.tab_bar = solid(BG_PANEL);
    c.tab = solid(BG_PANEL);
    c.tab_foreground = solid(TEXT_SECONDARY);
    c.tab_active = solid(BG_BASE);
    c.tab_active_foreground = solid(TEXT_PRIMARY);
    c.tab_bar_segmented = solid(BG_ELEVATED);

    c.title_bar = solid(BG_PANEL);
    c.title_bar_border = solid(BORDER);
    c.status_bar = solid(BG_PANEL);
    c.status_bar_border = solid(BORDER);
    c.tiles = solid(BG_BASE);

    c.popover = solid(BG_ELEVATED);
    c.popover_foreground = solid(TEXT_PRIMARY);
    c.group_box = solid(BG_PANEL);
    c.group_box_foreground = solid(TEXT_PRIMARY);
    c.accordion = solid(BG_PANEL);
    c.accordion_hover = solid(BG_HOVER);
    c.description_list_label = solid(BG_PANEL);
    c.description_list_label_foreground = solid(TEXT_SECONDARY);

    c.progress_bar = solid(ACCENT);
    c.slider_bar = solid(ACCENT);
    c.slider_thumb = solid(ACCENT);
    c.switch = solid(BORDER_STRONG);
    c.switch_thumb = solid(ACCENT);
    c.skeleton = solid(BG_HOVER);

    c.scrollbar = alpha(BG_BASE, 0.0);
    c.scrollbar_thumb = solid(BORDER_STRONG);
    c.scrollbar_thumb_hover = solid(0x404040);

    c.chart_1 = solid(TEXT_PRIMARY);
    c.chart_2 = solid(TEXT_SECONDARY);
    c.chart_3 = solid(TEXT_MUTED);
    c.chart_4 = solid(0x525252);
    c.chart_5 = solid(BORDER_STRONG);
    c.chart_bullish = solid(TEXT_PRIMARY);
    c.chart_bearish = solid(TEXT_MUTED);

    // Even the raw named colors collapse to grays so nothing in the UI can
    // ever render a hue.
    c.red = solid(0xD4D4D4);
    c.red_light = solid(0xE5E5E5);
    c.green = solid(TEXT_SECONDARY);
    c.green_light = solid(0xC0C0C0);
    c.blue = solid(TEXT_PRIMARY);
    c.blue_light = solid(ACCENT);
    c.yellow = solid(0xB5B5B5);
    c.yellow_light = solid(0xD4D4D4);
    c.magenta = solid(TEXT_SECONDARY);
    c.magenta_light = solid(0xC0C0C0);
    c.cyan = solid(0x8C8C8C);
    c.cyan_light = solid(TEXT_SECONDARY);
}

/// Grayscale syntax-highlight theme for the preview code editor: hierarchy
/// through luminance only, matching the monochrome rule (drive-usage
/// category colors remain the app's sole hue exception).
fn monochrome_highlight_theme() -> std::sync::Arc<gpui_component::highlighter::HighlightTheme> {
    // Zed-compatible theme JSON — gpui-component deserializes hex colors.
    const THEME_JSON: &str = r##"{
        "name": "piku-monochrome",
        "appearance": "dark",
        "style": {
            "editor.line_number": "#525252",
            "editor.active_line_number": "#A3A3A3",
            "editor.active_line.background": "#141414",
            "syntax": {
                "keyword": { "color": "#EDEDED" },
                "function": { "color": "#D4D4D4" },
                "type": { "color": "#D4D4D4" },
                "constructor": { "color": "#D4D4D4" },
                "string": { "color": "#A3A3A3" },
                "number": { "color": "#C9C9C9" },
                "boolean": { "color": "#C9C9C9" },
                "constant": { "color": "#C9C9C9" },
                "comment": { "color": "#737373" },
                "comment.doc": { "color": "#737373" },
                "property": { "color": "#B5B5B5" },
                "attribute": { "color": "#B5B5B5" },
                "variable": { "color": "#EDEDED" },
                "operator": { "color": "#8C8C8C" },
                "punctuation": { "color": "#8C8C8C" },
                "tag": { "color": "#D4D4D4" },
                "label": { "color": "#D4D4D4" },
                "embedded": { "color": "#EDEDED" },
                "emphasis": { "color": "#EDEDED" },
                "emphasis.strong": { "color": "#FFFFFF" },
                "title": { "color": "#FFFFFF" },
                "link_text": { "color": "#EDEDED" },
                "link_uri": { "color": "#A3A3A3" },
                "hint": { "color": "#737373" }
            }
        }
    }"##;
    match serde_json::from_str(THEME_JSON) {
        Ok(theme) => std::sync::Arc::new(theme),
        Err(error) => {
            // Never panic over a theme: fall back to the library default.
            tracing::warn!("monochrome highlight theme failed to parse: {error}");
            gpui_component::highlighter::HighlightTheme::default_dark()
        }
    }
}
