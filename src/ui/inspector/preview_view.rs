//! Renders [`PreviewContent`] into the inspector's preview box.
//!
//! Theme adherence rules for every surface in this file:
//! - only `cx.theme().*` tokens (plus the named grays in `theme/monochrome.rs`)
//! - every preview surface is `.rounded(cx.theme().radius)` on `muted`
//! - monospace via `.font_family("monospace")`, sizes `text_xs`/`text_sm` only
//! - no shadows, no new hex values outside `monochrome.rs`

use gpui::{
    AnyElement, AppContext as _, Context, ImageSource, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement, RenderImage, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, Window, canvas, div, img,
    prelude::FluentBuilder as _, px,
};
use std::sync::Arc;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    list::ListItem,
    text::TextView,
    tree::{TreeItem, TreeState, tree},
    v_flex,
};
use std::path::{Path, PathBuf};

use crate::app::assets::PikuIcon;
use crate::core::format::format_size;
use crate::preview::content::{ArchiveItem, HexRow, PreviewContent};
use crate::ui::inspector::inspector_panel::{CodeSyncKey, InspectorPanel};

/// Archive listings render at most this many rows (the loader already caps
/// what it reads; this caps what the non-virtualized panel draws).
const ARCHIVE_ROWS_SHOWN: usize = 200;

/// Render the loaded preview. Caller guarantees `panel.loaded` is `Some` and
/// matches the selected entry.
pub(super) fn render_preview_box(
    panel: &mut InspectorPanel,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // Take/put-back so the content can be borrowed while the panel is
    // mutated (code-editor sync, view toggles).
    let Some(loaded) = panel.loaded.take() else {
        return div().into_any_element();
    };
    let element = match &loaded.content {
        PreviewContent::Image { path, dimensions } => {
            image_block(panel, path, *dimensions, cx)
        }
        PreviewContent::Code { text, language, truncated, total_size } => v_flex()
            .w_full()
            .gap_1()
            .when_truncated(*truncated, text.len(), *total_size, cx)
            .child(code_editor_block(
                panel,
                CodeSyncKey { path: loaded.path.clone(), variant: "code" },
                text,
                *language,
                window,
                cx,
            ))
            .into_any_element(),
        PreviewContent::Markdown { source, truncated } => {
            markdown_block(panel, &loaded.path, source, *truncated, window, cx)
        }
        PreviewContent::Structured { text, language, pretty, truncated } => {
            structured_block(panel, &loaded.path, text, *language, pretty.as_ref(), *truncated, window, cx)
        }
        PreviewContent::Archive { entries, total_count, truncated } => {
            archive_block(entries, *total_count, *truncated, cx)
        }
        PreviewContent::Audio { rows, waveform, duration_ms } => {
            audio_block(&loaded.path, rows, waveform, *duration_ms, cx)
        }
        PreviewContent::Video { rows, poster } => {
            video_block(&loaded.path, rows, poster.clone(), cx)
        }
        PreviewContent::Pdf { pages, total_pages, note } => {
            pdf_block(pages, *total_pages, note.as_ref(), cx)
        }
        PreviewContent::Hex { rows, signature, total_size } => {
            hex_block(rows, *signature, *total_size, cx)
        }
        PreviewContent::TooLarge { size } => message_box(
            format!("Too large to preview ({})", format_size(*size)),
            cx,
        ),
        PreviewContent::Error(message) => message_box(message.to_string(), cx),
    };
    panel.loaded = Some(loaded);
    element
}

// -- Code / text ------------------------------------------------------------

/// Lazily create the shared read-only code editor and sync it with `text`
/// when the (path, variant) key changed since the last render.
fn code_editor_block(
    panel: &mut InspectorPanel,
    key: CodeSyncKey,
    text: &SharedString,
    language: Option<&'static str>,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let state = match panel.code_state.clone() {
        Some(state) => state,
        None => {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor("text")
                    .line_number(true)
                    .soft_wrap(false)
            });
            panel.code_state = Some(state.clone());
            state
        }
    };
    if panel.code_synced.as_ref() != Some(&key) {
        let text = text.clone();
        state.update(cx, |input, cx| {
            input.set_highlighter(language.unwrap_or("text"), cx);
            input.set_value(text, window, cx);
        });
        panel.code_synced = Some(key);
    }

    div()
        .w_full()
        .h(px(320.))
        .rounded(cx.theme().radius)
        .overflow_hidden()
        .child(Input::new(&state).disabled(true).h_full().w_full())
        .into_any_element()
}

fn markdown_block(
    panel: &mut InspectorPanel,
    path: &Path,
    source: &SharedString,
    truncated: bool,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let raw = panel.view.markdown_raw;
    let toggle = h_flex()
        .gap_1()
        .child(toggle_button("pv-md-rendered", PikuIcon::Eye, "Rendered", !raw, cx, |view| {
            view.markdown_raw = false;
        }))
        .child(toggle_button("pv-md-raw", PikuIcon::FileText, "Raw source", raw, cx, |view| {
            view.markdown_raw = true;
        }));

    let body = if raw {
        code_editor_block(
            panel,
            CodeSyncKey { path: path.to_path_buf(), variant: "md-raw" },
            source,
            Some("markdown"),
            window,
            cx,
        )
    } else {
        div()
            .w_full()
            .p_2()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .text_sm()
            .child(TextView::markdown("pv-md", source.clone()))
            .into_any_element()
    };

    v_flex()
        .w_full()
        .gap_1()
        .child(toggle)
        .when_truncated(truncated, source.len(), 0, cx)
        .child(body)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn structured_block(
    panel: &mut InspectorPanel,
    path: &Path,
    text: &SharedString,
    language: Option<&'static str>,
    pretty: Option<&SharedString>,
    truncated: bool,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    // A tree needs a fully-parsed document, so it is only offered for the
    // formats we can parse and only when the head was not truncated.
    let tree_capable = matches!(ext.as_str(), "json" | "yaml" | "yml" | "toml") && !truncated;
    let tree_active = tree_capable && panel.view.structured_tree;
    let pretty_active = !tree_active && panel.view.json_pretty && pretty.is_some();

    let mut toggles = h_flex().gap_1();
    if tree_capable {
        toggles = toggles.child(toggle_button(
            "pv-struct-tree",
            PikuIcon::ListTree,
            "Tree",
            tree_active,
            cx,
            |view| {
                view.structured_tree = true;
            },
        ));
    }
    if pretty.is_some() {
        toggles = toggles
            .child(toggle_button(
                "pv-json-pretty",
                PikuIcon::Braces,
                "Pretty",
                pretty_active,
                cx,
                |view| {
                    view.structured_tree = false;
                    view.json_pretty = true;
                },
            ))
            .child(toggle_button(
                "pv-json-raw",
                PikuIcon::FileText,
                "Raw source",
                !pretty_active && !tree_active,
                cx,
                |view| {
                    view.structured_tree = false;
                    view.json_pretty = false;
                },
            ));
    } else if tree_capable {
        toggles = toggles.child(toggle_button(
            "pv-struct-raw",
            PikuIcon::FileText,
            "Raw source",
            !tree_active,
            cx,
            |view| {
                view.structured_tree = false;
            },
        ));
    }

    let mut column = v_flex().w_full().gap_1();
    if tree_capable || pretty.is_some() {
        column = column.child(toggles);
    }

    if tree_active {
        return column
            .child(structured_tree_block(panel, path, text, &ext, cx))
            .into_any_element();
    }

    let (shown, variant): (&SharedString, &'static str) = match pretty {
        Some(pretty) if pretty_active => (pretty, "structured-pretty"),
        _ => (text, "structured"),
    };
    column
        .when_truncated(truncated, text.len(), 0, cx)
        .child(code_editor_block(
            panel,
            CodeSyncKey { path: path.to_path_buf(), variant },
            shown,
            language,
            window,
            cx,
        ))
        .into_any_element()
}

// -- Structured tree --------------------------------------------------------

/// Cap on tree nodes built, so a pathological document can't explode the UI.
const TREE_NODE_BUDGET: usize = 5_000;

fn structured_tree_block(
    panel: &mut InspectorPanel,
    path: &Path,
    text: &SharedString,
    ext: &str,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // Rebuild only when the file changed, so expand/collapse state survives
    // re-renders (the panel re-renders as audio plays, on hover, etc.).
    if panel.tree_synced.as_deref() != Some(path) || panel.tree_state.is_none() {
        let items = parse_structured(text, ext).map(|value| {
            let mut budget = TREE_NODE_BUDGET;
            value_to_items(&value, &mut 0usize, &mut budget)
        });
        match items {
            Some(items) if !items.is_empty() => {
                let state = cx.new(|cx| TreeState::new(cx).items(items));
                panel.tree_state = Some(state);
            }
            _ => {
                panel.tree_state = None;
            }
        }
        panel.tree_synced = Some(path.to_path_buf());
    }

    let Some(state) = panel.tree_state.clone() else {
        return message_box("Could not parse this file into a tree.".to_string(), cx);
    };

    div()
        .w_full()
        .h(px(360.))
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .overflow_hidden()
        .child(tree(&state, |ix, entry, selected, _window, _cx| {
            ListItem::new(ix)
                .selected(selected)
                .pl(px(8.0 + 14.0 * entry.depth() as f32))
                .child(entry.item().label.clone())
        }))
        .into_any_element()
}

/// Parse a structured document into a common `serde_json::Value` model.
fn parse_structured(text: &str, ext: &str) -> Option<serde_json::Value> {
    match ext {
        "json" => serde_json::from_str(text).ok(),
        "toml" => serde_json::to_value(toml::from_str::<toml::Value>(text).ok()?).ok(),
        "yaml" | "yml" => {
            serde_json::to_value(serde_yml::from_str::<serde_yml::Value>(text).ok()?).ok()
        }
        _ => None,
    }
}

fn value_to_items(
    value: &serde_json::Value,
    next_id: &mut usize,
    budget: &mut usize,
) -> Vec<TreeItem> {
    match value {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, val)| tree_node(key, val, next_id, budget).expanded(true))
            .collect(),
        serde_json::Value::Array(arr) => arr
            .iter()
            .enumerate()
            .map(|(i, val)| tree_node(&format!("[{i}]"), val, next_id, budget).expanded(true))
            .collect(),
        other => vec![tree_node("value", other, next_id, budget)],
    }
}

fn tree_node(
    key: &str,
    value: &serde_json::Value,
    next_id: &mut usize,
    budget: &mut usize,
) -> TreeItem {
    *next_id += 1;
    let id = format!("n{next_id}");
    if *budget == 0 {
        return TreeItem::new(id, "…".to_string());
    }
    *budget -= 1;
    match value {
        serde_json::Value::Object(map) => TreeItem::new(id, format!("{key}  {{{}}}", map.len()))
            .children(map.iter().map(|(k, v)| tree_node(k, v, next_id, budget))),
        serde_json::Value::Array(arr) => TreeItem::new(id, format!("{key}  [{}]", arr.len()))
            .children(
                arr.iter()
                    .enumerate()
                    .map(|(i, v)| tree_node(&format!("[{i}]"), v, next_id, budget)),
            ),
        scalar => TreeItem::new(id, format!("{key}: {}", scalar_label(scalar))),
    }
}

fn scalar_label(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            let clipped: String = s.chars().take(80).collect();
            if clipped.len() < s.len() {
                format!("\"{clipped}…\"")
            } else {
                format!("\"{clipped}\"")
            }
        }
        _ => String::new(),
    }
}

// -- Image ------------------------------------------------------------------

fn image_block(
    panel: &mut InspectorPanel,
    path: &Path,
    dimensions: Option<(u32, u32)>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let fit = panel.view.image_fit || dimensions.is_none();
    let zoom = panel.view.image_zoom;
    let path: PathBuf = path.to_path_buf();

    let content: AnyElement = if fit {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_1()
            .child(
                img(path)
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element()
    } else {
        // 1:1 / zoomed: explicit size inside a scrollable viewport — panning
        // comes free from the scroll container.
        let (w, h) = dimensions.unwrap_or((0, 0));
        div()
            .absolute()
            .inset_0()
            .id("pv-image-pan")
            .overflow_x_scroll()
            .overflow_y_scroll()
            .child(
                img(path)
                    .w(px(w as f32 * zoom))
                    .h(px(h as f32 * zoom))
                    .flex_none(),
            )
            .into_any_element()
    };

    let sizeable = dimensions.is_some();
    let viewport = div()
        .relative()
        .w_full()
        .h(px(240.))
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .overflow_hidden()
        .child(div().absolute().inset_0().child(checkerboard()))
        .child(content)
        // Wheel zoom: active when zoomed in (1:1 mode) or with Ctrl held while
        // fitted, so plain scrolling still pans a zoomed image. Only meaningful
        // when the pixel dimensions are known (needed to size the 1:1 image).
        .when(sizeable, |el| {
            el.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if !(event.modifiers.control || this.view.image_fit) {
                    return;
                }
                let dy = event.delta.pixel_delta(px(16.)).y;
                if dy == px(0.) {
                    return;
                }
                let factor = if dy > px(0.) { 1.25 } else { 0.8 };
                this.view.image_fit = false;
                this.view.image_zoom = (this.view.image_zoom * factor).clamp(0.1, 8.0);
                cx.notify();
            }))
        });
    let mut controls = h_flex()
        .gap_1()
        .items_center()
        .child(
            Button::new("pv-img-fit")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Maximize)
                .tooltip("Fit to window")
                .selected(fit)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_fit = true;
                    cx.notify();
                })),
        )
        .child(
            Button::new("pv-img-actual")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Scan)
                .tooltip("Actual size (1:1)")
                .selected(!fit)
                .disabled(!sizeable)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_fit = false;
                    this.view.image_zoom = 1.0;
                    cx.notify();
                })),
        )
        .child(
            Button::new("pv-img-out")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ZoomOut)
                .tooltip("Zoom out")
                .disabled(!sizeable)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_fit = false;
                    this.view.image_zoom = (this.view.image_zoom * 0.8).clamp(0.1, 8.0);
                    cx.notify();
                })),
        )
        .child(
            Button::new("pv-img-in")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ZoomIn)
                .tooltip("Zoom in")
                .disabled(!sizeable)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_fit = false;
                    this.view.image_zoom = (this.view.image_zoom * 1.25).clamp(0.1, 8.0);
                    cx.notify();
                })),
        );
    if let Some((w, h)) = dimensions {
        let label = if fit {
            format!("{w} × {h}")
        } else {
            format!("{w} × {h} · {:.0}%", zoom * 100.0)
        };
        controls = controls.child(
            div()
                .ml_auto()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        );
    }

    v_flex()
        .w_full()
        .gap_1()
        .child(viewport)
        .child(controls)
        .into_any_element()
}

/// Transparency backdrop painted directly — two grays from the monochrome
/// palette, 8px cells, bounded iteration.
fn checkerboard() -> impl IntoElement {
    canvas(
        |_, _, _| (),
        |bounds, _, window, _| {
            const CELL: f32 = 8.0;
            let base = crate::theme::solid(crate::theme::CHECKER_A);
            let alt = crate::theme::solid(crate::theme::CHECKER_B);
            window.paint_quad(gpui::fill(bounds, base));
            let cols = ((f32::from(bounds.size.width) / CELL).ceil() as usize).min(120);
            let rows = ((f32::from(bounds.size.height) / CELL).ceil() as usize).min(120);
            for row in 0..rows {
                for col in 0..cols {
                    if (row + col) % 2 == 0 {
                        continue;
                    }
                    let origin = gpui::point(
                        bounds.origin.x + px(col as f32 * CELL),
                        bounds.origin.y + px(row as f32 * CELL),
                    );
                    window.paint_quad(gpui::fill(
                        gpui::Bounds { origin, size: gpui::size(px(CELL), px(CELL)) },
                        alt,
                    ));
                }
            }
        },
    )
    .size_full()
}

// -- Hex / archive / media --------------------------------------------------

fn hex_block(
    rows: &[HexRow],
    signature: Option<&'static str>,
    total_size: u64,
    cx: &Context<InspectorPanel>,
) -> AnyElement {
    let header = format!(
        "{} · {}",
        signature.unwrap_or("Unknown format"),
        format_size(total_size)
    );
    v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(header),
        )
        .child(
            v_flex()
                .w_full()
                .font_family("monospace")
                .text_xs()
                .children(rows.iter().map(|row| {
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground)
                                .child(row.offset.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(cx.theme().foreground)
                                .child(row.hex.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_color(cx.theme().muted_foreground)
                                .child(row.ascii.clone()),
                        )
                })),
        )
        .into_any_element()
}

fn archive_block(
    entries: &[ArchiveItem],
    total_count: usize,
    truncated: bool,
    cx: &Context<InspectorPanel>,
) -> AnyElement {
    let mut header = format!("{total_count} entries");
    if truncated {
        header.push_str(&format!(" · first {} listed", entries.len()));
    }
    let shown = entries.len().min(ARCHIVE_ROWS_SHOWN);
    let mut list = v_flex().w_full().gap_0p5();
    for item in &entries[..shown] {
        list = list.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(if item.is_dir { IconName::Folder } else { IconName::File })
                        .size(px(13.))
                        .text_color(cx.theme().muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(cx.theme().foreground)
                        .child(item.name.clone()),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(if item.is_dir { "—".to_string() } else { format_size(item.size) }),
                ),
        );
    }
    if entries.len() > shown {
        list = list.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("…and {} more", entries.len() - shown)),
        );
    }
    v_flex()
        .w_full()
        .p_2()
        .gap_1()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().foreground)
                .child(header),
        )
        .child(list)
        .into_any_element()
}

// -- Audio ------------------------------------------------------------------

/// Playable audio: the shared interactive waveform + transport (play/seek/
/// volume), a button to pop out a dockable media panel, then the tag rows.
fn audio_block(
    path: &Path,
    rows: &[(SharedString, SharedString)],
    waveform: &[f32],
    duration_ms: u64,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let meta = crate::ui::media::transport::track_meta_from(path, rows, duration_ms);
    v_flex()
        .w_full()
        .gap_2()
        .child(crate::ui::media::transport::audio_transport(
            "pv-audio",
            path,
            meta,
            waveform,
            duration_ms,
            cx,
        ))
        .child(open_in_panel_button("pv-audio-pop", path, cx))
        .child(media_block(rows, cx))
        .into_any_element()
}

/// Small "open in a dockable media panel" button, shared by audio + video.
fn open_in_panel_button(
    id: &'static str,
    path: &Path,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let path = path.to_path_buf();
    h_flex()
        .gap_1()
        .child(
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(PikuIcon::Maximize2)
                .label("Open in media panel")
                .tooltip("Open in a dockable media panel")
                .on_click(cx.listener(move |_, _, window, cx| {
                    window.dispatch_action(
                        Box::new(crate::app::actions::OpenMediaPanel(path.clone())),
                        cx,
                    );
                })),
        )
        .into_any_element()
}

// -- Video ------------------------------------------------------------------

fn video_block(
    path: &Path,
    rows: &[(SharedString, SharedString)],
    poster: Option<Arc<RenderImage>>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let mut column = v_flex().w_full().gap_2();
    if let Some(image) = poster {
        column = column.child(
            div()
                .relative()
                .w_full()
                .h(px(240.))
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .overflow_hidden()
                .child(
                    div().absolute().inset_0().flex().items_center().justify_center().child(
                        img(ImageSource::Render(image))
                            .max_w_full()
                            .max_h_full()
                            .object_fit(ObjectFit::Contain),
                    ),
                ),
        );
    }
    let path_buf = path.to_path_buf();
    column
        .child(
            h_flex().gap_1().child(
                Button::new("pv-video-open")
                    .ghost()
                    .xsmall()
                    .icon(PikuIcon::ExternalLink)
                    .label("Play in default app")
                    .tooltip("Open in your system's video player")
                    .on_click(cx.listener(move |_, _, _, _| {
                        let _ = open::that_detached(&path_buf);
                    })),
            ),
        )
        .child(open_in_panel_button("pv-video-pop", path, cx))
        .child(media_block(rows, cx))
        .into_any_element()
}

// -- PDF --------------------------------------------------------------------

fn pdf_block(
    pages: &[Arc<RenderImage>],
    total_pages: usize,
    note: Option<&SharedString>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    if pages.is_empty() {
        let message = note
            .map(|note| note.to_string())
            .unwrap_or_else(|| "No preview available.".to_string());
        return message_box(message, cx);
    }

    let mut column = v_flex().w_full().gap_2().child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!("{total_pages} page{}", if total_pages == 1 { "" } else { "s" })),
    );
    // Pages stack vertically; the inspector's outer scroll container walks them.
    for page in pages {
        column = column.child(
            div()
                .w_full()
                .h(px(520.))
                .rounded(cx.theme().radius)
                .bg(cx.theme().muted)
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    img(ImageSource::Render(page.clone()))
                        .max_w_full()
                        .max_h_full()
                        .object_fit(ObjectFit::Contain),
                ),
        );
    }
    if let Some(note) = note {
        column = column.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(note.clone()),
        );
    }
    column.into_any_element()
}

fn media_block(rows: &[(SharedString, SharedString)], cx: &Context<InspectorPanel>) -> AnyElement {
    v_flex()
        .w_full()
        .p_2()
        .gap_2()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .children(rows.iter().map(|(label, value)| {
            v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label.clone()),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().foreground)
                        .child(value.clone()),
                )
        }))
        .into_any_element()
}

fn message_box(message: String, cx: &Context<InspectorPanel>) -> AnyElement {
    div()
        .w_full()
        .h(px(120.))
        .p_2()
        .flex()
        .items_center()
        .justify_center()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            div()
                .text_xs()
                .text_center()
                .text_color(cx.theme().muted_foreground)
                .child(message),
        )
        .into_any_element()
}

// -- Small helpers ----------------------------------------------------------

fn toggle_button(
    id: &'static str,
    icon: PikuIcon,
    tooltip: &'static str,
    active: bool,
    cx: &mut Context<InspectorPanel>,
    apply: impl Fn(&mut super::inspector_panel::PreviewViewState) + 'static,
) -> Button {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(icon)
        .tooltip(tooltip)
        .selected(active)
        .on_click(cx.listener(move |this, _, _, cx| {
            apply(&mut this.view);
            cx.notify();
        }))
}

/// Chainable truncation banner.
trait TruncatedExt: Sized + ParentElement {
    fn when_truncated(
        self,
        truncated: bool,
        shown_bytes: usize,
        total: u64,
        cx: &Context<InspectorPanel>,
    ) -> Self {
        if !truncated {
            return self;
        }
        let mut text = format!("Showing the first {}", format_size(shown_bytes as u64));
        if total > 0 {
            text.push_str(&format!(" of {}", format_size(total)));
        }
        let mut this = self;
        this.extend([div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(text)
            .into_any_element()]);
        this
    }
}

impl TruncatedExt for gpui::Div {}
