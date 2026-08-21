//! Renders [`PreviewContent`] into the inspector's Preview tab.
//!
//! Every block here is handed a **real bounded box** — the tab is
//! `flex_1().min_h_0()` and, unlike the Details tab, is not wrapped in a scroll
//! container. So `size_full()` means the panel, and a block sizes itself one of
//! two ways:
//!
//! - **Fills**: image, code, structured tree, video poster. `flex_1().min_h_0()`
//!   for the content, `flex_none` for any toolbar or banner above or below it.
//! - **Scrolls**: hex rows, archive listings, media metadata, PDF pages, diffs.
//!   These are as tall as their content, so [`scroll_fill`] gives them the
//!   scroll container and the padding the tab itself does not provide.
//!
//! There is deliberately no `fill_height(window)`-style guess any more. Deriving
//! a height from the window was wrong by the height of the media bar whenever
//! something was playing; the tab measures its own box instead.
//!
//! Theme adherence rules for every surface in this file:
//! - only `cx.theme().*` tokens (plus the named grays in `theme/monochrome.rs`)
//! - filling surfaces run edge to edge and are therefore **not** rounded — a
//!   radius against the panel edge reads as a seam. Surfaces inside a scrolling
//!   block keep `.rounded(cx.theme().radius)` on `muted`.
//! - monospace via `.font_family("monospace")`, sizes `text_xs`/`text_sm` only
//! - no shadows, no new hex values outside `monochrome.rs`

use gpui::{
    AnyElement, AppContext as _, Context, ImageSource, InteractiveElement as _, IntoElement,
    ObjectFit, ParentElement, RenderImage, ScrollWheelEvent, SharedString,
    StatefulInteractiveElement as _, Styled, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _, px,
};
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
use std::path::Path;
use std::sync::Arc;

use crate::app::assets::PikuIcon;
use crate::backend::services::preview::providers::image::PREVIEW_MAX_EDGE;
use crate::core::format::format_size;
use crate::preview::content::{ArchiveItem, HexRow, PreviewContent, PreviewImage};
use crate::ui::inspector::inspector_panel::{CodeSyncKey, ImageZoom, InspectorPanel, fit_scale};

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
    let element = match &*loaded.content {
        PreviewContent::Image { source, dimensions } => image_block(panel, source, *dimensions, cx),
        PreviewContent::Code {
            text,
            language,
            truncated,
            total_size,
        } => v_flex()
            .size_full()
            .when_truncated(*truncated, text.len(), *total_size, cx)
            .child(code_editor_block(
                panel,
                CodeSyncKey {
                    path: loaded.path.clone(),
                    variant: "code",
                },
                text,
                *language,
                window,
                cx,
            ))
            .into_any_element(),
        PreviewContent::Markdown { source, truncated } => {
            markdown_block(panel, &loaded.path, source, *truncated, window, cx)
        }
        PreviewContent::Structured {
            text,
            language,
            pretty,
            truncated,
        } => structured_block(
            panel,
            &loaded.path,
            text,
            *language,
            pretty.as_ref(),
            *truncated,
            window,
            cx,
        ),
        PreviewContent::Archive {
            entries,
            total_count,
            truncated,
        } => scroll_fill(
            "pv-archive-scroll",
            archive_block(entries, *total_count, *truncated, cx),
            cx,
        ),
        PreviewContent::Audio {
            rows,
            waveform,
            duration_ms,
        } => scroll_fill(
            "pv-audio-scroll",
            audio_block(&loaded.path, rows, waveform, *duration_ms, cx),
            cx,
        ),
        PreviewContent::Video { rows, poster } => {
            video_block(&loaded.path, rows, poster.clone(), cx)
        }
        PreviewContent::Pdf {
            pages,
            total_pages,
            note,
        } => pdf_block(panel, pages, *total_pages, note.as_ref(), cx),
        PreviewContent::Hex {
            rows,
            signature,
            total_size,
        } => scroll_fill(
            "pv-hex-scroll",
            hex_block(rows, *signature, *total_size, cx),
            cx,
        ),
        PreviewContent::TooLarge { size } => {
            message_box(format!("Too large to preview ({})", format_size(*size)), cx)
        }
        PreviewContent::Diff(payload) => scroll_fill("pv-diff-scroll", diff_block(payload, cx), cx),
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

    // `flex_1().min_h_0()`, not an explicit height. The Preview tab hands this
    // a real bounded box, so "the rest of the column" is a height the layout
    // can resolve — which is what replaced deriving one from the window size.
    div()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden()
        .child(Input::new(&state).disabled(true).h_full().w_full())
        .into_any_element()
}

/// Wrap a natural-height block so it fills the Preview tab and scrolls if it
/// overflows.
///
/// Some previews have a height of their own (an image fills; a code editor
/// fills) and some are lists that are as tall as their content (hex rows, an
/// archive listing, media metadata). The second kind needs the scroll container
/// the tab deliberately does not provide, plus the padding the tab deliberately
/// drops so filling previews can run edge to edge.
fn scroll_fill(id: &'static str, content: AnyElement, cx: &Context<InspectorPanel>) -> AnyElement {
    div()
        .id(id)
        .size_full()
        .overflow_y_scroll()
        .p_3()
        .bg(cx.theme().sidebar)
        .child(content)
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
        .child(toggle_button(
            "pv-md-rendered",
            PikuIcon::Eye,
            "Rendered",
            !raw,
            cx,
            |view| {
                view.markdown_raw = false;
            },
        ))
        .child(toggle_button(
            "pv-md-raw",
            PikuIcon::FileText,
            "Raw source",
            raw,
            cx,
            |view| {
                view.markdown_raw = true;
            },
        ));

    let body = if raw {
        code_editor_block(
            panel,
            CodeSyncKey {
                path: path.to_path_buf(),
                variant: "md-raw",
            },
            source,
            Some("markdown"),
            window,
            cx,
        )
    } else {
        // A rendered document is as tall as its content, so it gets the scroll
        // container rather than being pinned to the viewport and clipped.
        div()
            .id("pv-md-scroll")
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_y_scroll()
            .p_2()
            .text_sm()
            .child(TextView::markdown("pv-md", source.clone()))
            .into_any_element()
    };

    v_flex()
        .size_full()
        .child(div().flex_none().px_2().pt_2().child(toggle))
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

    let mut column = v_flex().size_full();
    if tree_capable || pretty.is_some() {
        column = column.child(div().flex_none().px_2().pt_2().child(toggles));
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
            CodeSyncKey {
                path: path.to_path_buf(),
                variant,
            },
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
        .flex_1()
        .min_h_0()
        .w_full()
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
    image: &PreviewImage,
    dimensions: Option<(u32, u32)>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // SVG has no pixel size of its own, so there is nothing to be 1:1 *with*;
    // it stays fitted and the renderer rasterizes it at whatever size it lands.
    let sizeable = dimensions.is_some();
    let zoom = if sizeable {
        panel.view.image_zoom
    } else {
        ImageZoom::Fit
    };
    // Measured last frame. `None` on the very first frame for a given panel
    // size — which is exactly why fitted mode does not depend on it.
    let fitted = dimensions.and_then(|d| fit_scale(panel.preview_viewport.get(), d));

    // A path clone or an `Arc` bump. Never a re-read, and never a re-decode:
    // for anything but SVG the pixels were decoded on a worker, with the
    // orientation already applied.
    let source: ImageSource = match image {
        PreviewImage::Path(path) => ImageSource::from(path.clone()),
        PreviewImage::Decoded(frame) => ImageSource::Render(frame.clone()),
    };

    let content: AnyElement = if zoom.is_fit() {
        // Fitted mode is the renderer's own `Contain`, not a scale we compute.
        // It needs no measurement, so it is correct on the first frame and
        // stays correct while the dock is being dragged — a computed scale
        // would be one frame behind for the whole drag.
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .p_1()
            .child(
                img(source.clone())
                    .max_w_full()
                    .max_h_full()
                    .object_fit(ObjectFit::Contain),
            )
            .into_any_element()
    } else {
        // 1:1 / zoomed: explicit size inside a scrollable viewport — panning
        // comes free from the scroll container.
        let (w, h) = dimensions.unwrap_or((0, 0));
        let scale = zoom.scale(fitted).unwrap_or(1.0);
        div()
            .absolute()
            .inset_0()
            .id("pv-image-pan")
            .overflow_x_scroll()
            .overflow_y_scroll()
            .child(
                img(source.clone())
                    .w(px(w as f32 * scale))
                    .h(px(h as f32 * scale))
                    .flex_none(),
            )
            .into_any_element()
    };

    let viewport = div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        // The transparency backdrop, in the same two monochrome grays as
        // before. This used to be a hand-painted canvas whose loop was capped
        // at 120 cells per axis — 960 px, which never mattered while the box
        // was a fixed 240 px tall and would have left the bottom of a
        // full-height panel flat. GPUI's checkerboard is a background pattern:
        // no cell count to cap, and one draw instead of 14 400 quads.
        //
        // The shader paints its colour on alternating cells and leaves the
        // rest transparent, so the two grays are two layers — the base here,
        // the pattern over it.
        .bg(crate::theme::solid(crate::theme::CHECKER_A))
        .overflow_hidden()
        .child(div().absolute().inset_0().bg(gpui::checkerboard(
            crate::theme::solid(crate::theme::CHECKER_B),
            CHECKER_CELL_PX,
        )))
        .child(content)
        // Wheel zoom: active when zoomed in (1:1 mode) or with Ctrl held while
        // fitted, so plain scrolling still pans a zoomed image. Only meaningful
        // when the pixel dimensions are known (needed to size the 1:1 image).
        .when(sizeable, |el| {
            el.on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                if !(event.modifiers.control || this.view.image_zoom.is_fit()) {
                    return;
                }
                let dy = event.delta.pixel_delta(px(16.)).y;
                if dy == px(0.) {
                    return;
                }
                let factor = if dy > px(0.) { 1.25 } else { 0.8 };
                this.view.image_zoom = this.view.image_zoom.stepped(factor, fitted);
                cx.notify();
            }))
        });

    let mut controls = h_flex()
        .flex_none()
        .gap_1()
        .items_center()
        .px_2()
        .py_1()
        .child(
            Button::new("pv-img-fit")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Maximize)
                .tooltip("Fit to window")
                .selected(zoom.is_fit())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_zoom = ImageZoom::Fit;
                    cx.notify();
                })),
        )
        .child(
            Button::new("pv-img-actual")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Scan)
                .tooltip("Actual size (1:1)")
                .selected(zoom == ImageZoom::Actual)
                .disabled(!sizeable)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.view.image_zoom = ImageZoom::Actual;
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view.image_zoom = this.view.image_zoom.stepped(0.8, fitted);
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
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view.image_zoom = this.view.image_zoom.stepped(1.25, fitted);
                    cx.notify();
                })),
        );
    if let Some((w, h)) = dimensions {
        // The scale now has a number in *both* modes, because fitted resolves
        // against the measured viewport. Before, fitted simply had no readout.
        let mut label = match zoom.scale(fitted) {
            Some(scale) => format!("{w} × {h} · {:.0}%", scale * 100.0),
            None => format!("{w} × {h}"),
        };
        if zoom_is_resolution_limited(zoom.scale(fitted), (w, h)) {
            // Say so rather than quietly showing a soft image. The preview
            // buffer is capped at PREVIEW_MAX_EDGE for the cache's sake, so
            // past this point magnification is stretching decoded pixels, not
            // revealing source ones.
            label.push_str(" · limited");
        }
        controls = controls.child(
            div()
                .ml_auto()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        );
    }

    v_flex()
        .size_full()
        .child(viewport)
        .child(controls)
        .into_any_element()
}

/// Checkerboard cell size, in pixels.
const CHECKER_CELL_PX: f32 = 8.0;

/// Whether the displayed scale is asking for more source pixels than the
/// preview buffer actually holds.
///
/// The buffer's longest edge is capped at `PREVIEW_MAX_EDGE` so one image
/// cannot evict the whole preview cache. An image larger than that is decoded
/// downscaled, and `dimensions` reports the *source* size — so "100 %" on a
/// 6000 px photo is already a 2048 px buffer stretched threefold. A 240 px box
/// hid that; a full-height panel does not.
///
/// This is specifically about detail the *file* has and the preview does not.
/// Magnifying a small image past 100 % is ordinary upscaling — the file has no
/// more detail either, so there is nothing to warn about, and saying otherwise
/// would put the marker on every tiny icon the moment it was fitted.
fn zoom_is_resolution_limited(scale: Option<f32>, dimensions: (u32, u32)) -> bool {
    let Some(scale) = scale else {
        return false;
    };
    let longest = dimensions.0.max(dimensions.1);
    if longest <= PREVIEW_MAX_EDGE {
        return false;
    }
    // The buffer holds `PREVIEW_MAX_EDGE` pixels across a source that is
    // `longest` wide, so it runs out at that ratio.
    scale > PREVIEW_MAX_EDGE as f32 / longest as f32
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
                    Icon::new(if item.is_dir {
                        IconName::Folder
                    } else {
                        IconName::File
                    })
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
                        .child(if item.is_dir {
                            "—".to_string()
                        } else {
                            format_size(item.size)
                        }),
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
    let mut column = v_flex().size_full();
    if let Some(image) = poster {
        // The poster takes the room the metadata does not, rather than a fixed
        // 240 px — a video preview is mostly the frame.
        column = column.child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .w_full()
                .bg(cx.theme().muted)
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
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
            h_flex().flex_none().gap_1().px_2().pt_2().child(
                Button::new("pv-video-open")
                    .ghost()
                    .xsmall()
                    .icon(PikuIcon::ExternalLink)
                    .label("Play in default app")
                    .tooltip("Open in your system's video player")
                    // Through `shell_open`, not `open::that_detached`
                    // directly: it re-authorizes the *resolved* target, so a
                    // link inside an allowed root cannot hand the OS handler
                    // something outside it.
                    .on_click(cx.listener(move |_, _, window, cx| {
                        let name = path_buf
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        crate::ui::explorer::shell_open(&name, &path_buf, window, cx);
                    })),
            ),
        )
        .child(
            div()
                .flex_none()
                .px_2()
                .child(open_in_panel_button("pv-video-pop", path, cx)),
        )
        .child(
            div()
                .id("pv-video-meta")
                .flex_none()
                .max_h(px(180.))
                .overflow_y_scroll()
                .p_2()
                .child(media_block(rows, cx)),
        )
        .into_any_element()
}

// -- PDF --------------------------------------------------------------------

/// Minimum height for a PDF page before the viewport has been measured. Only
/// ever used for the first frame after the tab opens.
const PDF_PAGE_MIN_HEIGHT: f32 = 320.0;

fn pdf_block(
    panel: &InspectorPanel,
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

    // A page is as tall as the tab, so scrolling moves one page at a time.
    // Measured, not guessed: the old version subtracted a hard-coded 180 px of
    // "chrome" from the whole window, which was wrong by exactly the height of
    // the media bar whenever something was playing.
    let page_height = f32::from(panel.preview_viewport.get().size.height).max(PDF_PAGE_MIN_HEIGHT);

    let mut column = v_flex().w_full().gap_2().child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(format!(
                "{total_pages} page{}",
                if total_pages == 1 { "" } else { "s" }
            )),
    );
    for page in pages {
        column = column.child(
            div()
                .w_full()
                .h(px(page_height))
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
    // Pages stack vertically and this walks them.
    scroll_fill("pv-pdf-scroll", column.into_any_element(), cx)
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

// -- Git diff ---------------------------------------------------------------

/// Monochrome unified diff: hunk headers muted, removed lines muted, added
/// lines at full foreground on a subtle band. Also used directly by the Git
/// inspector mode. All strings are pre-sanitized by the git backend.
pub(super) fn diff_block(
    payload: &crate::services::git::types::DiffPayload,
    cx: &Context<InspectorPanel>,
) -> AnyElement {
    use crate::services::git::types::DiffLineKind;

    if let Some(note) = &payload.note {
        return message_box(note.clone(), cx);
    }

    let mut body = v_flex().w_full().gap_1().child(
        h_flex()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(gpui_component::Icon::new(crate::app::assets::PikuIcon::GitCommit).size(px(12.)))
            .child(format!("{} → {}", payload.old_label, payload.new_label)),
    );

    for (hunk_ix, hunk) in payload.hunks.iter().enumerate() {
        let mut block = v_flex()
            .w_full()
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .p_1()
            .font_family("monospace")
            .text_xs()
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(hunk.header.clone()),
            );
        for (ix, (kind, text)) in hunk.lines.iter().enumerate() {
            let (prefix, color, banded) = match kind {
                DiffLineKind::Context => (" ", cx.theme().muted_foreground, false),
                DiffLineKind::Del => ("-", cx.theme().muted_foreground, false),
                DiffLineKind::Add => ("+", cx.theme().foreground, true),
            };
            let mut line = h_flex()
                .id(("diff-line", hunk_ix * 10_000 + ix))
                .w_full()
                .gap_1()
                .child(div().w(px(10.)).flex_none().text_color(color).child(prefix))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(color)
                        .whitespace_nowrap()
                        .child(text.clone()),
                );
            if banded {
                line = line.bg(cx.theme().list_active);
            }
            if *kind == DiffLineKind::Del {
                line = line.line_through();
            }
            block = block.child(line);
        }
        body = body.child(
            div()
                .id(("diff-hunk", hunk_ix))
                .overflow_x_scroll()
                .child(block),
        );
    }

    if payload.truncated {
        body = body.child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("… diff truncated"),
        );
    }
    body.into_any_element()
}

fn message_box(message: String, cx: &Context<InspectorPanel>) -> AnyElement {
    div()
        .size_full()
        .p_2()
        .flex()
        .items_center()
        .justify_center()
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

#[cfg(test)]
mod resolution_tests {
    use super::*;

    /// An image small enough to be decoded whole is never "limited": the
    /// preview holds every pixel the file does.
    #[test]
    fn an_undownscaled_image_is_never_resolution_limited() {
        assert!(!zoom_is_resolution_limited(Some(0.5), (800, 600)));
        assert!(!zoom_is_resolution_limited(Some(1.0), (800, 600)));
        // Even a 16px icon fitted to a big panel, which is a large scale.
        assert!(!zoom_is_resolution_limited(Some(40.0), (16, 16)));
    }

    /// A photo past the decode cap is limited exactly when magnification
    /// outruns the buffer, not before.
    #[test]
    fn a_downscaled_photo_is_limited_past_the_buffer_ratio() {
        let dims = (PREVIEW_MAX_EDGE * 3, PREVIEW_MAX_EDGE * 2);
        // The buffer covers a third of the source, so up to 1/3 scale is exact.
        assert!(!zoom_is_resolution_limited(Some(0.3), dims));
        assert!(zoom_is_resolution_limited(Some(0.4), dims));
        assert!(zoom_is_resolution_limited(Some(1.0), dims));
    }

    /// No measured viewport means no number to judge, so no claim either way.
    #[test]
    fn an_unmeasured_viewport_makes_no_claim() {
        assert!(!zoom_is_resolution_limited(None, (9000, 9000)));
    }
}
