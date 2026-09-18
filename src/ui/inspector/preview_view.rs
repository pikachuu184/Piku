//! Renders [`PreviewContent`] into the inspector's Preview tab.
//!
//! Every block here is handed a **real bounded box** — the tab is
//! `flex_1().min_h_0()` and, unlike the Details tab, is not wrapped in a scroll
//! container. So `size_full()` means the panel, and a block sizes itself one of
//! two ways:
//!
//! - **Fills**: image, PDF page, code, structured tree, video poster.
//!   `flex_1().min_h_0()` for the content, `flex_none` for any toolbar or banner
//!   above or below it.
//! - **Scrolls**: hex rows, archive listings, media metadata, diffs. These are
//!   as tall as their content, so [`scroll_fill`] gives them the scroll container
//!   and the padding the tab itself does not provide.
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
    text::{TextView, TextViewState},
    tree::{TreeItem, TreeState, tree},
    v_flex,
};
use std::path::Path;
use std::sync::Arc;

use crate::app::assets::PikuIcon;
use crate::backend::services::preview::FrameSpec;
use crate::backend::services::preview::providers::image::PREVIEW_MAX_EDGE;
use crate::core::format::format_size;
use crate::preview::content::{ArchiveItem, HexRow, PreviewContent, PreviewImage};
use crate::ui::inspector::inspector_panel::{CodeSyncKey, InspectorPanel};
use crate::ui::inspector::viewport::ImageZoom;

/// Archive listings render at most this many rows (the loader already caps
/// what it reads; this caps what the non-virtualized panel draws).
const ARCHIVE_ROWS_SHOWN: usize = 200;

// -- The controller dispatch ------------------------------------------------

/// What a preview surface can do.
///
/// One declaration per surface. The body reads these directly —
/// [`zoomable_surface`] gates the wheel on `zoomable` and the drag on `pannable`
/// — and the toolbar half is held to them by
/// `a_surface_that_can_be_acted_on_offers_controls`, which fails if a surface
/// claims something the user can act on and [`controls_for`] gives them nowhere
/// to act on it. So every flag here gates real behaviour or is asserted against
/// real behaviour. A flag that does neither is a claim rather than a capability,
/// and three of them used to be exactly that.
///
/// Two the request named are deliberately absent, because nothing here could
/// enforce them: `playable` has no state to gate — the audio and video blocks
/// render their transport unconditionally and there is no such thing as audio
/// that does not play — and text selection belongs to `InputState`, which owns
/// it. This is the table both land in the moment there is behaviour to gate.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(super) struct Capabilities {
    /// Has a scale the user can change.
    pub zoomable: bool,
    /// Can be dragged around inside its viewport.
    pub pannable: bool,
    /// Is a sequence of pages the user can step through.
    pub paginated: bool,
    /// Offers alternative renderings of the same bytes (raw/rendered,
    /// tree/pretty/raw).
    pub switchable: bool,
}

/// The kinds of preview surface, one per way of drawing a file.
///
/// This exists so [`render_preview_box`] and [`preview_actions`] start from the
/// same value instead of matching [`PreviewContent`] separately. They did, and
/// they drifted: the PDF block had `pages` and `total_pages` from the day it was
/// written, while the toolbar's match fell through to `_ => None`, so there was
/// no way to reach page two. Both now call [`Self::of`] on the same content, and
/// every match over [`PreviewContent`] in this file is exhaustive with no
/// catch-all — so a new content type cannot be added without deciding both how
/// it draws and what it lets the user do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PreviewSurface {
    Image,
    Code,
    Markdown,
    Structured,
    Archive,
    Audio,
    Video,
    Pdf,
    Hex,
    Diff,
    /// A sentence in a box: too large, or an error.
    Message,
}

impl PreviewSurface {
    pub fn of(content: &PreviewContent) -> Self {
        match content {
            PreviewContent::Image { .. } => Self::Image,
            PreviewContent::Code { .. } => Self::Code,
            PreviewContent::Markdown { .. } => Self::Markdown,
            PreviewContent::Structured { .. } => Self::Structured,
            PreviewContent::Archive { .. } => Self::Archive,
            PreviewContent::Audio { .. } => Self::Audio,
            PreviewContent::Video { .. } => Self::Video,
            PreviewContent::Pdf { .. } => Self::Pdf,
            PreviewContent::Hex { .. } => Self::Hex,
            PreviewContent::Diff(_) => Self::Diff,
            PreviewContent::TooLarge { .. } | PreviewContent::Error(_) => Self::Message,
        }
    }

    /// Every variant, for table tests. Adding one without extending this fails
    /// `every_surface_names_its_control_group`.
    ///
    /// `allow`, not `expect`: exercised by the tests below but not by the
    /// binary, so an expectation would be unfulfilled in the test target. Same
    /// reasoning as `PreviewKind::ALL` on the provider side.
    #[allow(dead_code, reason = "table-test vocabulary")]
    pub const ALL: [PreviewSurface; 11] = [
        Self::Image,
        Self::Code,
        Self::Markdown,
        Self::Structured,
        Self::Archive,
        Self::Audio,
        Self::Video,
        Self::Pdf,
        Self::Hex,
        Self::Diff,
        Self::Message,
    ];

    pub fn capabilities(self) -> Capabilities {
        match self {
            // The image and the PDF page share one set of flags because they
            // share one engine. `viewport.rs`'s module doc has said so from the
            // start; this is where it became true. `Pdf` used to claim
            // `paginated` alone, which is why the page drew through a plain
            // `ObjectFit::Contain` and neither wheel nor drag reached it.
            Self::Image | Self::Pdf => Capabilities {
                zoomable: true,
                pannable: true,
                paginated: matches!(self, Self::Pdf),
                switchable: false,
            },
            Self::Markdown | Self::Structured => Capabilities {
                switchable: true,
                ..Capabilities::default()
            },
            // One way of drawing, nothing to steer.
            Self::Code
            | Self::Archive
            | Self::Audio
            | Self::Video
            | Self::Hex
            | Self::Diff
            | Self::Message => Capabilities::default(),
        }
    }

    /// Whether this surface may draw controls in the tab bar's suffix.
    ///
    /// "May", not "does": a structured document with nothing to switch between
    /// — no pretty form, and too truncated to build a tree — correctly draws
    /// none, and so does a PDF with no decodable page. This is the upper bound,
    /// and the assertion that matters is the other direction: a surface that
    /// claims a capability the user can act on must offer somewhere to act on
    /// it. `a_surface_that_can_be_acted_on_offers_controls` is that assertion.
    ///
    /// `allow` for the same reason as [`Self::ALL`]: the tests are the caller.
    #[allow(dead_code, reason = "asserted by the control-table tests")]
    pub fn may_have_controls(self) -> bool {
        let capabilities = self.capabilities();
        capabilities.zoomable || capabilities.paginated || capabilities.switchable
    }
}

/// Render the loaded preview. Caller guarantees `panel.loaded` is `Some` and
/// matches the selected entry.
pub(super) fn render_preview_box(
    panel: &mut InspectorPanel,
    window: &mut Window,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // An `Arc` bump and a path clone, so the content can be borrowed while the
    // panel is mutated (code-editor sync, view toggles).
    //
    // This used to take the whole `loaded` and put it back at the end, which
    // left `panel.loaded` as `None` for the entire body — so a block that asked
    // the panel about the file it was drawing (`drawn_metrics`, `drawn_frame`,
    // `has_own_pixels`) got the answer "there is no file", and one early return
    // anywhere in here would have dropped the preview on the floor.
    let Some((content, path)) = panel
        .loaded
        .as_ref()
        .map(|loaded| (loaded.content.clone(), loaded.path.clone()))
    else {
        return div().into_any_element();
    };
    // Derived once, here, and handed to the blocks that need it. A block that
    // named its own surface would be one more place for the body and the
    // capability table to disagree, which is the whole failure this dispatch
    // exists to prevent.
    let surface = PreviewSurface::of(&content);
    match &*content {
        PreviewContent::Image { source, .. } => image_block(panel, surface, source, cx),
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
                    path: path.clone(),
                    variant: "code",
                },
                text,
                *language,
                window,
                cx,
            ))
            .into_any_element(),
        PreviewContent::Markdown { source, truncated } => {
            markdown_block(panel, &path, source, *truncated, window, cx)
        }
        PreviewContent::Structured {
            text,
            language,
            pretty,
            truncated,
        } => structured_block(
            panel,
            &path,
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
            audio_block(&path, rows, waveform, *duration_ms, cx),
            cx,
        ),
        PreviewContent::Video { poster, .. } => video_block(&path, poster.clone(), cx),
        PreviewContent::Pdf { pages, note, .. } => {
            pdf_block(panel, surface, pages, note.as_ref(), cx)
        }
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
    }
}

// -- Tab-bar actions --------------------------------------------------------

/// The controls for the current preview, rendered in the tab bar's suffix so
/// they sit on the right of the same row as the tabs.
///
/// They used to be a strip inside each block — a toggle row above markdown and
/// structured data, a button row under the image. Hoisting them here means the
/// preview surface itself is nothing but content, edge to edge, and that the
/// controls land in the same place whatever the file is.
///
/// The dispatch is [`controls_for`], which is exhaustive over
/// [`PreviewContent`]. The `_ => None` it replaced is exactly how PDF ended up
/// with pages and no way to turn them.
pub(super) fn preview_actions(
    panel: &InspectorPanel,
    cx: &mut Context<InspectorPanel>,
) -> Option<AnyElement> {
    // An `Arc` bump, so the borrow of `panel` ends before `cx` is used to
    // build listeners.
    let loaded = panel.loaded.as_ref()?;
    let content = loaded.content.clone();
    let ext = loaded
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let surface = PreviewSurface::of(&content);
    match controls_for(&content, &ext) {
        // The image and the PDF page share the zoom row because they share the
        // viewport behind it; the PDF adds its page counter in front.
        PreviewControls::Image => Some(zoom_controls(panel, surface, cx)),
        PreviewControls::Pdf { total_pages } => Some(pdf_actions(panel, surface, total_pages, cx)),
        PreviewControls::Markdown => Some(markdown_actions(panel, cx)),
        PreviewControls::Structured {
            tree_capable,
            has_pretty,
        } => Some(structured_actions(panel, tree_capable, has_pretty, cx)),
        PreviewControls::None => None,
    }
}

/// Which toolbar a preview gets, and the facts that toolbar needs.
///
/// One decision, made in one place, answering both "is there a toolbar?" and
/// "what is in it?". Those used to be two matches — one over [`PreviewSurface`]
/// in [`preview_actions`], another buried inside each builder — and the second
/// could contradict the first: `structured_actions` returned `Option` so it
/// could decide, five arguments deep, that there was nothing to switch between.
///
/// Exhaustive over [`PreviewContent`] with no catch-all, so a new content type
/// cannot be added without deciding what it lets the user do.
pub(super) fn controls_for(content: &PreviewContent, ext: &str) -> PreviewControls {
    match content {
        // Including SVG, whose buttons are all disabled but present: the row
        // vanishing between one image and the next is worse than a dim row.
        PreviewContent::Image { .. } => PreviewControls::Image,
        PreviewContent::Pdf {
            pages, total_pages, ..
        } => match pages.is_empty() {
            // No decodable first page — pdfium missing, or the document past the
            // page cap. That is a message box, and a message has no pages.
            true => PreviewControls::None,
            false => PreviewControls::Pdf {
                total_pages: *total_pages,
            },
        },
        PreviewContent::Markdown { .. } => PreviewControls::Markdown,
        PreviewContent::Structured {
            pretty, truncated, ..
        } => {
            let tree_capable = structured_tree_capable(ext, *truncated);
            let has_pretty = pretty.is_some();
            match tree_capable || has_pretty {
                true => PreviewControls::Structured {
                    tree_capable,
                    has_pretty,
                },
                // Neither a tree nor a pretty form: one rendering, so a "Raw
                // source" toggle would be a button that toggles nothing.
                false => PreviewControls::None,
            }
        }
        PreviewContent::Code { .. }
        | PreviewContent::Archive { .. }
        | PreviewContent::Audio { .. }
        | PreviewContent::Video { .. }
        | PreviewContent::Hex { .. }
        | PreviewContent::Diff(_)
        | PreviewContent::TooLarge { .. }
        | PreviewContent::Error(_) => PreviewControls::None,
    }
}

/// The outcome of [`controls_for`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum PreviewControls {
    Image,
    Pdf {
        total_pages: usize,
    },
    Markdown,
    Structured {
        tree_capable: bool,
        has_pretty: bool,
    },
    /// Nothing to steer: one way of drawing, or a degenerate document.
    None,
}

fn markdown_actions(panel: &InspectorPanel, cx: &mut Context<InspectorPanel>) -> AnyElement {
    let raw = panel.view.markdown_raw;
    h_flex()
        .gap_1()
        .child(toggle_button(
            "pv-md-rendered",
            PikuIcon::Eye,
            "Rendered",
            !raw,
            cx,
            |view| view.markdown_raw = false,
        ))
        .child(toggle_button(
            "pv-md-raw",
            PikuIcon::FileText,
            "Raw source",
            raw,
            cx,
            |view| view.markdown_raw = true,
        ))
        .into_any_element()
}

/// The raw/pretty/tree toggles, for the tab bar's suffix.
///
/// Both flags come from [`controls_for`], which has already established that at
/// least one of them is true — so this always has something to draw and returns
/// an element rather than an `Option`.
fn structured_actions(
    panel: &InspectorPanel,
    tree_capable: bool,
    has_pretty: bool,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let tree_active = tree_capable && panel.view.structured_tree;
    let pretty_active = !tree_active && panel.view.json_pretty && has_pretty;

    let mut row = h_flex().gap_1();
    if tree_capable {
        row = row.child(toggle_button(
            "pv-struct-tree",
            PikuIcon::ListTree,
            "Tree",
            tree_active,
            cx,
            |view| view.structured_tree = true,
        ));
    }
    if has_pretty {
        row = row
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
    } else {
        row = row.child(toggle_button(
            "pv-struct-raw",
            PikuIcon::FileText,
            "Raw source",
            !tree_active,
            cx,
            |view| view.structured_tree = false,
        ));
    }
    row.into_any_element()
}

/// Shared by the actions row and the block itself, so the toggle can never
/// offer a mode the block would not render.
pub(super) fn structured_tree_capable(ext: &str, truncated: bool) -> bool {
    matches!(ext, "json" | "yaml" | "yml" | "toml") && !truncated
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
    let body = if panel.view.markdown_raw {
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
        rendered_markdown_block(panel, path, source, cx)
    };

    v_flex()
        .size_full()
        .when_truncated(truncated, source.len(), 0, cx)
        .child(body)
        .into_any_element()
}

/// The rendered document.
///
/// Two decisions here are load-bearing for whether it scrolls at all, and both
/// were wrong before:
///
/// * **The state is an entity, parsed once per file.** `TextView::markdown(id,
///   text)` owns no state, so it creates one through `use_keyed_state` — which
///   also installs an observation that re-renders this whole panel on every
///   `TextViewState` notify. Passing a state entity skips that path entirely,
///   along with a per-frame `str == str` comparison over up to 512 KiB.
/// * **`scrollable(true)` is what turns virtualization on.** Without it,
///   `TextView` renders every block of the document into an element on every
///   frame; with it, the document goes through `gpui::list` and only the
///   visible blocks are built. It brings its own scrollbar, which is why there
///   is deliberately no `overflow_y_scroll` wrapper here — an outer scroll
///   container would both double-scroll *and* put `cx.notify(InspectorPanel)`
///   on every wheel tick, rebuilding the document at input rate.
///
/// The flag must be set on the `TextView`, not on the state: `request_layout`
/// assigns `state.scrollable = self.scrollable` on every layout, so the
/// element's value wins.
fn rendered_markdown_block(
    panel: &mut InspectorPanel,
    path: &Path,
    source: &SharedString,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let state = match panel.md_state.clone() {
        Some(state) => state,
        None => {
            let text = source.clone();
            let state = cx.new(|cx| TextViewState::markdown(text.as_str(), cx));
            panel.md_state = Some(state.clone());
            panel.md_synced = Some(path.to_path_buf());
            state
        }
    };
    if panel.md_synced.as_deref() != Some(path) {
        let text = source.clone();
        state.update(cx, |state, cx| state.set_text(text.as_str(), cx));
        panel.md_synced = Some(path.to_path_buf());
    }

    div()
        .flex_1()
        .min_h_0()
        .w_full()
        .p_2()
        .text_sm()
        .child(TextView::new(&state).scrollable(true))
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
    // The mode toggles live in the tab bar's suffix; this only has to agree
    // with them about which modes exist, which `structured_tree_capable` is
    // shared to guarantee.
    let tree_capable = structured_tree_capable(&ext, truncated);
    let tree_active = tree_capable && panel.view.structured_tree;
    let pretty_active = !tree_active && panel.view.json_pretty && pretty.is_some();

    let column = v_flex().size_full();

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
    panel: &InspectorPanel,
    surface: PreviewSurface,
    image: &PreviewImage,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // The turned frame when one has arrived for this file, otherwise the payload
    // the loader cached. `drawn_frame` keys on the path alone, not on the exact
    // turn, so the image holds the angle you last chose while the next one
    // renders — half a second of the previous angle beats half a second of
    // nothing, and unlike the PDF's page counter there is no label here that
    // could be wrong about it.
    //
    // Either arm is a path clone or an `Arc` bump. Never a re-read, and never a
    // re-decode in the UI: for anything but SVG the pixels were decoded on a
    // worker, with the orientation already applied.
    let source: ImageSource = match panel.drawn_frame() {
        Some(frame) => ImageSource::Render(frame.image.clone()),
        None => match image {
            PreviewImage::Path(path) => ImageSource::from(path.clone()),
            PreviewImage::Decoded(frame) => ImageSource::Render(frame.clone()),
        },
    };

    v_flex()
        .size_full()
        .child(zoomable_surface(
            panel,
            source,
            Backdrop::Checker,
            surface.capabilities(),
            cx,
        ))
        .into_any_element()
}

/// What sits behind the pixels in a [`zoomable_surface`].
enum Backdrop {
    /// The two-gray transparency checkerboard, for an image that may have an
    /// alpha channel.
    Checker,
    /// A flat muted fill, for a PDF page. Paper is opaque, and a checkerboard
    /// behind a white page reads as damage rather than as transparency.
    Flat,
}

/// One zoomable, pannable viewport — the image and the PDF page both draw here.
///
/// This is the function `viewport.rs`'s module doc has described from the start.
/// It was not true: the PDF page had its own `ObjectFit::Contain` div, so Fit,
/// Fit Width, Actual Size, wheel zoom and drag pan reached the image and stopped
/// there. Both callers now hand their pixels to this, and what is enabled comes
/// from `capabilities` — which is what makes the capability table load-bearing
/// instead of decorative.
///
/// Returns a `flex_1` element, so **every caller must wrap it in a flex column**.
/// `flex_1` with no flex parent is given a share of nothing and resolves to zero
/// height, which draws nothing and leaves a zero-sized hitbox that silently
/// swallows the wheel.
fn zoomable_surface(
    panel: &InspectorPanel,
    source: ImageSource,
    backdrop: Backdrop,
    capabilities: Capabilities,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // `None` when there is no geometry to anchor a gesture against: an SVG, which
    // has no pixel size of its own and so nothing to be 1:1 *with*, or a box the
    // renderer has not measured yet. Both gestures need it, so both wait for it —
    // this is the honest reading of the `let sizeable = dimensions.is_some()`
    // that used to stand in for the capability table here.
    let metrics = panel.drawn_metrics();
    let can_zoom = capabilities.zoomable && metrics.is_some();
    let can_pan = capabilities.pannable && metrics.is_some();

    // Untouched, or not yet measured: hand it to the renderer's own `Contain`.
    // That needs no measurement, so it is correct on the very first frame and
    // stays correct while the dock is being dragged, where a computed scale
    // would be one frame behind for the whole of the drag.
    let transformed = metrics.filter(|_| !panel.view.viewport.is_untransformed());

    let content: AnyElement = match transformed {
        None => div()
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
            .into_any_element(),
        Some(metrics) => {
            // Explicitly sized and explicitly offset. Centring is the flex
            // parent's job and `pan` displaces it from there, so zero pan is
            // centred at *every* scale — which is what the scroll container
            // this replaced could not express (see `viewport.rs`).
            let scale = panel
                .view
                .viewport
                .effective_scale(Some(metrics))
                .unwrap_or(1.0);
            let (width, height) = metrics.displayed();
            let pan = panel.view.viewport.pan;
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .overflow_hidden()
                .child(
                    img(source.clone())
                        .flex_none()
                        .relative()
                        .left(pan.x)
                        .top(pan.y)
                        .w(px(width as f32 * scale))
                        .h(px(height as f32 * scale)),
                )
                .into_any_element()
        }
    };

    let viewport_box = div()
        .relative()
        .flex_1()
        .min_h_0()
        .w_full()
        .overflow_hidden();
    let viewport_box = match backdrop {
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
        Backdrop::Checker => viewport_box
            .bg(crate::theme::solid(crate::theme::CHECKER_A))
            .child(div().absolute().inset_0().bg(gpui::checkerboard(
                crate::theme::solid(crate::theme::CHECKER_B),
                CHECKER_CELL_PX,
            ))),
        Backdrop::Flat => viewport_box.bg(cx.theme().muted),
    };

    let viewport = viewport_box
        .child(content)
        // Ctrl/Cmd + wheel zooms toward the cursor; a plain wheel pans, and
        // shift makes it pan sideways. This is the convention Zed's image
        // viewer uses and the one every document viewer uses.
        //
        // It replaced an unconditional zoom, which was itself a fix for a
        // version that zoomed only while fitted — where the first notch worked
        // and every one after it silently became a pan. The modifier settles
        // that properly: both gestures now work at every zoom level, and which
        // one you get is something you chose rather than something the current
        // state decided for you.
        //
        // One handler, two capabilities: it is attached when either gesture is
        // available and each branch checks its own, so a surface that could pan
        // but not zoom would still answer the wheel.
        .when(can_zoom || can_pan, |el| {
            el.on_scroll_wheel(cx.listener(move |this, event: &ScrollWheelEvent, _, cx| {
                let delta = event.delta.pixel_delta(px(16.));
                if event.modifiers.control || event.modifiers.platform {
                    if !can_zoom || delta.y == px(0.) {
                        return;
                    }
                    let factor = if delta.y > px(0.) { 1.25 } else { 0.8 };
                    this.view
                        .viewport
                        .zoom_about(event.position, factor, metrics);
                } else if can_pan {
                    // A wheel with no horizontal axis still pans sideways when
                    // shift is held, which is how a mouse reaches a wide image.
                    let pan = match event.modifiers.shift && delta.x == px(0.) {
                        true => gpui::point(delta.y, px(0.)),
                        false => delta,
                    };
                    if pan.x == px(0.) && pan.y == px(0.) {
                        return;
                    }
                    this.view.viewport.pan_by(pan, metrics);
                }
                cx.notify();
            }))
        })
        // Click-drag pans. `MouseMoveEvent` carries no delta, so the previous
        // position is kept on the viewport and differenced here; `dragging()`
        // keeps a plain click from nudging it.
        .when(can_pan, |el| {
            el.on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, _| {
                    this.view.viewport.drag_from = Some(event.position);
                }),
            )
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseUpEvent, _, _| {
                    this.view.viewport.drag_from = None;
                }),
            )
            .on_mouse_move(cx.listener(
                move |this, event: &gpui::MouseMoveEvent, _, cx| {
                    if !event.dragging() {
                        this.view.viewport.drag_from = None;
                        return;
                    }
                    let Some(from) = this.view.viewport.drag_from.replace(event.position) else {
                        return;
                    };
                    // Unconditional, unlike the version this replaced, which
                    // refused to drag while fitted. The clamp already pins an
                    // axis with nothing to pan to, so a drag that cannot move
                    // anything now simply does not move anything.
                    this.view.viewport.pan_by(
                        gpui::point(event.position.x - from.x, event.position.y - from.y),
                        metrics,
                    );
                    cx.notify();
                },
            ))
        });

    // No control strip: the controls live in the tab bar's suffix now, so the
    // pixels are the entire tab.
    viewport.into_any_element()
}

/// The zoom, rotate and readout controls, for the tab bar's suffix.
///
/// Shared by the image and the PDF page because they share the viewport behind
/// them. Every button calls an [`InspectorPanel`] method rather than touching
/// the viewport here — the *same* method its keyboard shortcut calls, so the two
/// cannot drift apart, and each one anchors on the viewport centre. Assigning
/// the level and leaving the pan alone is what made button zoom drift while
/// wheel zoom did not.
///
/// `surface` decides one thing: whether the pixels on screen can be fewer than
/// the size the readout names. See the `limited` marker below.
fn zoom_controls(
    panel: &InspectorPanel,
    surface: PreviewSurface,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let zoom = panel.view.viewport.zoom;
    let dimensions = panel.drawn_dimensions();
    let metrics = panel.drawn_metrics();
    // False for exactly one previewable thing: an SVG. It has no pixel size of
    // its own, so there is nothing to be 1:1 with, nothing to fit a width
    // against, and nothing here for a worker to turn.
    let sizeable = panel.has_own_pixels();

    let mut row = h_flex()
        .gap_1()
        .items_center()
        .child(
            Button::new("pv-zoom-fit")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Maximize)
                .tooltip("Fit to window")
                .selected(zoom == ImageZoom::Fit)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_fit(cx))),
        )
        .child(
            Button::new("pv-zoom-fit-width")
                .ghost()
                .xsmall()
                .icon(PikuIcon::FitWidth)
                .tooltip("Fit width")
                .selected(zoom == ImageZoom::FitWidth)
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_fit_width(cx))),
        )
        .child(
            Button::new("pv-zoom-actual")
                .ghost()
                .xsmall()
                .icon(PikuIcon::Scan)
                .tooltip("Actual size (1:1)")
                .selected(zoom == ImageZoom::Actual)
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_actual_size(cx))),
        )
        .child(
            Button::new("pv-zoom-out")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ZoomOut)
                .tooltip("Zoom out")
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_zoom_by(0.8, cx))),
        )
        .child(
            Button::new("pv-zoom-in")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ZoomIn)
                .tooltip("Zoom in")
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_zoom_by(1.25, cx))),
        )
        .child(
            // Turning the pixels, not transforming the element: gpui's
            // `with_transformation` exists on `svg` and not on `img`, so the
            // frame is re-rendered on a worker. That is also why this is
            // disabled for an SVG — the one image with no buffer to turn.
            Button::new("pv-zoom-rotate")
                .ghost()
                .xsmall()
                .icon(PikuIcon::RotateCw)
                .tooltip("Rotate 90° clockwise")
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_rotate(cx))),
        )
        .child(
            // Distinct from Fit: this clears the rotation as well, so it is the
            // one button that puts a turned page back the right way up.
            Button::new("pv-zoom-reset")
                .ghost()
                .xsmall()
                .icon(PikuIcon::RefreshCw)
                .tooltip("Reset view")
                .disabled(!sizeable)
                .on_click(cx.listener(move |this, _, _, cx| this.preview_reset_view(cx))),
        );

    if let Some((w, h)) = dimensions {
        // The scale has a number in *both* modes, because fitted resolves
        // against the measured viewport. Before, fitted simply had no readout.
        let scale = panel.view.viewport.effective_scale(metrics);
        let mut label = match scale {
            Some(scale) => format!("{w} × {h} · {:.0}%", scale * 100.0),
            None => format!("{w} × {h}"),
        };
        // Say so rather than quietly showing a soft image. The preview buffer is
        // capped at PREVIEW_MAX_EDGE for the cache's sake, so past this point
        // magnification is stretching decoded pixels, not revealing source ones.
        //
        // Image only. A PDF page has no source pixels to fall short of — pdfium
        // rasterized it at the size reported here — so the same arithmetic
        // applied to a tall page would put the marker on a frame that holds
        // every pixel it claims to.
        if surface == PreviewSurface::Image && zoom_is_resolution_limited(scale, (w, h)) {
            label.push_str(" · limited");
        }
        row = row.child(
            div()
                .pl_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        );
    }
    row.into_any_element()
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
    poster: Option<Arc<RenderImage>>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    let path_buf = path.to_path_buf();
    let mut column = v_flex().size_full().gap_2();
    // Controls first, in a single compact row pinned at the top, so they stay
    // visible and well-fitted rather than floating at the bottom of a
    // full-height poster (which is what happened once the metadata was removed).
    column = column.child(
        h_flex()
            .flex_none()
            .flex_wrap()
            .gap_1()
            .px_2()
            .pt_2()
            .child(
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
                            .map(|n| n.to_string_lossy().into_owned()) // raw-path-ok: shell_open sanitizes it
                            .unwrap_or_default();
                        crate::ui::explorer::shell_open(&name, &path_buf, window, cx);
                    })),
            )
            .child(open_in_panel_button("pv-video-pop", path, cx)),
    );
    if let Some(image) = poster {
        // The poster fills the room below the controls — a video preview is
        // mostly the frame.
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
    column.into_any_element()
}

// -- PDF --------------------------------------------------------------------

fn pdf_block(
    panel: &InspectorPanel,
    surface: PreviewSurface,
    pages: &[Arc<RenderImage>],
    note: Option<&SharedString>,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    if pages.is_empty() {
        // Nothing decoded at all: pdfium missing, or the document past the page
        // cap. The note says which.
        let message = note
            .map(|note| note.to_string())
            .unwrap_or_else(|| "No preview available.".to_string());
        return message_box(message, cx);
    }

    // One page, filling the tab, with the navigation in the tab bar's suffix.
    // The previous version stacked every decoded page into a scroll column,
    // which laid out up to twelve full-height page elements on every frame to
    // show one of them, and left `total_pages` as the only way to know where
    // you were.
    let mut column = v_flex().size_full();
    if let Some(note) = note {
        column = column.child(
            div()
                .flex_none()
                .w_full()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(note.clone()),
        );
    }

    // The *exact* frame, not merely one for this file. The toolbar beside it
    // reads "7 / 40", and a page number is a claim about what you are looking
    // at — so a page still rendering waits behind a spinner rather than showing
    // page 6 under a label that says 7. `image_block` deliberately does the
    // opposite: a turn has no counter that could be wrong about it.
    let wanted = FrameSpec::PdfPage {
        index: panel.view.page,
        // Normalized here rather than trusted: `==` below compares the raw turn
        // count, so a 4 that meant 0 would never match a frame holding 0 and the
        // page would sit behind a spinner for ever.
        quarter_turns: panel.view.rotation % 4,
    };
    // Page one, the right way up, is the frame the initial payload already
    // carries — so opening a PDF draws immediately and asks for nothing.
    let unturned = FrameSpec::PdfPage {
        index: 0,
        quarter_turns: 0,
    };
    let page = match panel.drawn_frame() {
        Some(frame) if frame.spec == wanted => Some(frame.image.clone()),
        _ if wanted == unturned => pages.first().cloned(),
        _ => None,
    };

    column
        .child(match page {
            Some(page) => zoomable_surface(
                panel,
                ImageSource::Render(page),
                Backdrop::Flat,
                surface.capabilities(),
                cx,
            ),
            // The same spinner on the same fill the whole tab shows while the
            // first page loads, because this is the same wait.
            None => div()
                .flex_1()
                .min_h_0()
                .w_full()
                .bg(cx.theme().muted)
                .flex()
                .items_center()
                .justify_center()
                .child(crate::ui::components::piku_spinner(
                    gpui_component::Size::Small,
                    cx,
                ))
                .into_any_element(),
        })
        .into_any_element()
}

/// Page navigation plus the shared zoom row, for the tab bar's suffix.
///
/// This is the control the two-match arrangement lost: the PDF block always had
/// `pages` and `total_pages`, and `preview_actions` returned `None` for it
/// regardless, so there was no way to reach page two. [`controls_for`] is what
/// stops that recurring — a document that reports pages has to return a toolbar,
/// and `a_surface_that_can_be_acted_on_offers_controls` is the test that says so.
fn pdf_actions(
    panel: &InspectorPanel,
    surface: PreviewSurface,
    total_pages: usize,
    cx: &mut Context<InspectorPanel>,
) -> AnyElement {
    // `total_pages` is the document's own count, and every page of it is
    // reachable: the provider rasterizes page one and the rest are fetched on
    // demand. The label used to read "1 / 12 of 400", because the other 388 were
    // never rendered and could not be asked for.
    let last = total_pages.saturating_sub(1);
    let index = panel.view.page.min(last);

    h_flex()
        .gap_1()
        .items_center()
        .child(
            Button::new("pv-pdf-prev")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ChevronLeft)
                .tooltip("Previous page")
                .disabled(index == 0)
                .on_click(cx.listener(|this, _, _, cx| this.preview_step_page(-1, cx))),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{} / {total_pages}", index + 1)),
        )
        .child(
            Button::new("pv-pdf-next")
                .ghost()
                .xsmall()
                .icon(PikuIcon::ChevronRight)
                .tooltip("Next page")
                .disabled(index >= last)
                .on_click(cx.listener(|this, _, _, cx| this.preview_step_page(1, cx))),
        )
        .child(zoom_controls(panel, surface, cx))
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

#[cfg(test)]
mod control_tests {
    use super::*;
    use crate::preview::content::PreviewImage;
    use crate::preview::image_util::render_image_from_bgra_bytes;

    /// One opaque pixel. Enough to stand in for a decoded frame: nothing in the
    /// control dispatch looks at pixels, only at whether there are any.
    fn one_pixel() -> Arc<RenderImage> {
        render_image_from_bgra_bytes(1, 1, vec![0, 0, 0, 255])
    }

    /// A **non-degenerate** payload for each surface — one carrying everything
    /// its controls need — with the extension a real file of that kind would
    /// have. The degenerate cases are asserted separately, because for those
    /// `controls_for` is allowed to offer nothing.
    fn sample(surface: PreviewSurface) -> (PreviewContent, &'static str) {
        match surface {
            PreviewSurface::Image => (
                PreviewContent::Image {
                    source: PreviewImage::Decoded(one_pixel()),
                    dimensions: Some((40, 20)),
                },
                "png",
            ),
            PreviewSurface::Code => (
                PreviewContent::Code {
                    text: "fn main() {}".into(),
                    language: Some("rust"),
                    truncated: false,
                    total_size: 12,
                },
                "rs",
            ),
            PreviewSurface::Markdown => (
                PreviewContent::Markdown {
                    source: "# hi".into(),
                    truncated: false,
                },
                "md",
            ),
            PreviewSurface::Structured => (
                PreviewContent::Structured {
                    text: "{}".into(),
                    language: Some("json"),
                    pretty: Some("{}".into()),
                    truncated: false,
                },
                "json",
            ),
            PreviewSurface::Archive => (
                PreviewContent::Archive {
                    entries: Vec::new(),
                    total_count: 0,
                    truncated: false,
                },
                "zip",
            ),
            PreviewSurface::Audio => (
                PreviewContent::Audio {
                    rows: Vec::new(),
                    waveform: Vec::new(),
                    duration_ms: 1_000,
                },
                "mp3",
            ),
            PreviewSurface::Video => (
                PreviewContent::Video {
                    rows: Vec::new(),
                    poster: None,
                },
                "mp4",
            ),
            PreviewSurface::Pdf => (
                PreviewContent::Pdf {
                    pages: vec![one_pixel()],
                    total_pages: 40,
                    note: None,
                },
                "pdf",
            ),
            PreviewSurface::Hex => (
                PreviewContent::Hex {
                    rows: Vec::new(),
                    signature: None,
                    total_size: 8,
                },
                "bin",
            ),
            PreviewSurface::Diff => (
                PreviewContent::Diff(crate::services::git::types::DiffPayload {
                    old_label: "HEAD".to_string(),
                    new_label: "working tree".to_string(),
                    hunks: Vec::new(),
                    truncated: false,
                    note: None,
                }),
                "rs",
            ),
            PreviewSurface::Message => (PreviewContent::TooLarge { size: 1 << 40 }, "iso"),
        }
    }

    /// The table has to describe the surface it claims to, or every assertion
    /// below is quietly testing the wrong content.
    #[test]
    fn every_surface_names_its_control_group() {
        for surface in PreviewSurface::ALL {
            let (content, _) = sample(surface);
            assert_eq!(
                PreviewSurface::of(&content),
                surface,
                "the sample content for {surface:?} is not that surface"
            );
        }
    }

    /// The claim `pdf_actions`' doc comment made for months with nothing behind
    /// it: a preview the user can act on has somewhere to act on it. PDF is why
    /// — it reported `paginated` while its toolbar arm was `_ => None`, so the
    /// document had forty pages and no way to reach the second.
    #[test]
    fn a_surface_that_can_be_acted_on_offers_controls() {
        for surface in PreviewSurface::ALL {
            let (content, ext) = sample(surface);
            let controls = controls_for(&content, ext);
            assert_eq!(
                controls != PreviewControls::None,
                surface.may_have_controls(),
                "{surface:?} claims {:?} but its controls are {controls:?}",
                surface.capabilities(),
            );
        }
    }

    /// Zoom without pan is a trap: magnify past the box and everything outside
    /// it is unreachable. Whatever claims one has to claim the other.
    #[test]
    fn anything_zoomable_is_also_pannable() {
        for surface in PreviewSurface::ALL {
            let capabilities = surface.capabilities();
            assert!(
                !capabilities.zoomable || capabilities.pannable,
                "{surface:?} can be zoomed but not panned"
            );
        }
    }

    /// Pagination counts the *document*, not what the loader decoded. One page
    /// is rasterized up front and the rest are fetched on demand, so a 40-page
    /// file must offer all 40 — the cap that stopped at twelve is gone.
    #[test]
    fn pagination_counts_the_document_rather_than_what_was_decoded() {
        let (content, ext) = sample(PreviewSurface::Pdf);
        assert_eq!(
            controls_for(&content, ext),
            PreviewControls::Pdf { total_pages: 40 }
        );
    }

    /// The other direction of `may_have_controls`: it is an upper bound, so a
    /// degenerate document is entitled to offer nothing.
    #[test]
    fn a_degenerate_document_offers_nothing() {
        // No decodable page: pdfium missing, or past the page cap. That is a
        // message box, and a message has no pages to turn.
        let empty = PreviewContent::Pdf {
            pages: Vec::new(),
            total_pages: 0,
            note: Some("Preview unavailable.".into()),
        };
        assert_eq!(controls_for(&empty, "pdf"), PreviewControls::None);

        // Truncated YAML: no tree, because the parse would be of half a
        // document, and no pretty form, which is JSON only. Both toggles would
        // toggle nothing.
        let truncated = PreviewContent::Structured {
            text: "a: 1".into(),
            language: Some("yaml"),
            pretty: None,
            truncated: true,
        };
        assert_eq!(controls_for(&truncated, "yaml"), PreviewControls::None);

        // The same bytes whole do get a tree, so the case above is about the
        // truncation and not about YAML.
        let whole = PreviewContent::Structured {
            text: "a: 1".into(),
            language: Some("yaml"),
            pretty: None,
            truncated: false,
        };
        assert_eq!(
            controls_for(&whole, "yaml"),
            PreviewControls::Structured {
                tree_capable: true,
                has_pretty: false,
            }
        );
    }
}
