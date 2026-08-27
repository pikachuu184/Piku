//! A dockable, splittable media panel.
//!
//! Unlike the inspector (which follows the selection), this panel is pinned to
//! one file the user explicitly opened. It reuses the preview loader for
//! decoding and the shared [`transport`](super::transport) widgets for audio,
//! so it stays in sync with the same global player as the inspector and the
//! bottom bar. Video stays OS-delegated: a prominent poster plus an "open in
//! your player" button.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext as _, Context, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div, px,
};
use gpui_component::{
    ActiveTheme as _, Icon,
    button::{Button, ButtonVariants as _},
    dock::{Panel, PanelControl, PanelEvent, PanelInfo, PanelState},
    h_flex, v_flex,
};

use crate::app::assets::PikuIcon;
use crate::backend::dispatch::BackendExt as _;
use crate::backend::error::BackendError;
use crate::backend::protocol::Inflight;
use crate::backend::services::preview::PreviewRequest;
use crate::preview::content::PreviewContent;
use crate::preview::{PreviewKind, kind_for_path};
use crate::state::PikuState;
use crate::ui::media::VideoView;
use crate::ui::media::transport;

pub struct MediaPanel {
    focus_handle: FocusHandle,
    path: Option<PathBuf>,
    loaded: Option<Arc<PreviewContent>>,
    loading: bool,
    /// Staleness guard: a slow decode from an earlier file is dropped.
    /// The live preview request; dropping it cancels the worker.
    preview_req: Option<Inflight>,
    /// In-app video player, created when a video file is opened (its own decode
    /// pipeline; the `loaded` content only supplies the metadata rows).
    video: Option<gpui::Entity<VideoView>>,
    _audio: Subscription,
}

impl MediaPanel {
    pub const PANEL_NAME: &'static str = "PikuMedia";

    pub fn new(_: &mut Window, cx: &mut Context<Self>) -> Self {
        // Re-render while audio plays so the waveform/transport advance.
        let audio = PikuState::global(cx).audio.clone();
        let audio_sub = cx.observe(&audio, |_, _, cx| cx.notify());
        Self {
            focus_handle: cx.focus_handle(),
            path: None,
            loaded: None,
            loading: false,
            preview_req: None,
            video: None,
            _audio: audio_sub,
        }
    }

    /// Construct already pointed at `path` (the on-open and layout-restore path).
    pub fn for_path(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut panel = Self::new(window, cx);
        panel.open(path, cx);
        panel
    }

    /// Point the panel at `path` and decode it off-thread.
    pub fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.path.as_deref() == Some(path.as_path()) {
            return;
        }
        self.path = Some(path.clone());
        self.loaded = None;

        // Spin up (or tear down) the in-app video player. Dropping the old one
        // stops its decode thread; a new one begins buffering immediately.
        self.video = matches!(kind_for_path(&path), PreviewKind::VideoMeta)
            .then(|| cx.new(|cx| VideoView::new(path.clone(), cx)));

        self.loading = true;
        let request = PreviewRequest::for_path(&path);
        // The panel used to stat the file here, on the UI thread, purely to
        // build a cache probe. The service now reports the key it actually read
        // under, so that stat is gone: a miss simply decodes.
        self.preview_req = Some(cx.backend_task_cancellable(
            move |backend| backend.preview().preview(request),
            move |this: &mut Self, result, cx| {
                this.preview_req = None;
                let content = match result {
                    Ok(Ok(ready)) => {
                        let content = Arc::new(PreviewContent::from(ready.payload));
                        let cache = PikuState::global(cx).preview_cache.clone();
                        // Not a bare `insert`: whatever this displaces still owns
                        // a sprite-atlas tile, and only `drop_image` frees it.
                        crate::services::preview_cache::cache_preview(
                            &cache,
                            ready.key,
                            content.clone(),
                            cx,
                        );
                        content
                    }
                    // Superseded: a newer `open` owns the panel now.
                    Ok(Err(error)) if error.is_cancelled() => return,
                    Err(error) if error.is_cancelled() => return,
                    Ok(Err(error)) => Arc::new(PreviewContent::Error(
                        BackendError::from(error).user_message().into(),
                    )),
                    Err(error) => Arc::new(PreviewContent::Error(error.user_message().into())),
                };
                this.loaded = Some(content);
                this.loading = false;
            },
        ));
    }

    fn file_name(&self) -> SharedString {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Media")
            .to_string()
            .into()
    }

    fn render_content(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(path) = self.path.clone() else {
            return message(cx, "No media loaded.");
        };
        if self.loading {
            return message(cx, "Loading…");
        }
        // `take`/put-back so the content can be read while `self`/`cx` are
        // borrowed mutably by the transport widgets.
        let content = self.loaded.take();
        let element = match content.as_deref() {
            Some(PreviewContent::Audio {
                rows,
                waveform,
                duration_ms,
            }) => {
                let meta = transport::track_meta_from(&path, rows, *duration_ms);
                v_flex()
                    .w_full()
                    .gap_3()
                    .child(artwork(cx, PikuIcon::Music, 200.))
                    .child(transport::audio_transport(
                        "mp-audio",
                        &path,
                        meta,
                        waveform,
                        *duration_ms,
                        cx,
                    ))
                    .child(rows_block(rows, cx))
                    .into_any_element()
            }
            Some(PreviewContent::Video { rows, .. }) => {
                let open_path = path.clone();
                let mut column = v_flex().w_full().gap_3();
                // The in-app player (created in `open`); falls back to a poster
                // tile if it somehow wasn't set up.
                match &self.video {
                    Some(video) => column = column.child(video.clone()),
                    None => column = column.child(artwork(cx, PikuIcon::Film, 200.)),
                }
                column
                    .child(
                        Button::new("mp-video-open")
                            .ghost()
                            .icon(PikuIcon::ExternalLink)
                            .label("Play in default app")
                            .tooltip("Open in your system's video player")
                            // See the note in `preview_view`: the OS handoff
                            // must go through the canonicalize-then-reauthorize
                            // path, not straight to `open::that_detached`.
                            .on_click(cx.listener(move |_, _, window, cx| {
                                let name = open_path
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                crate::ui::explorer::shell_open(&name, &open_path, window, cx);
                            })),
                    )
                    .child(rows_block(rows, cx))
                    .into_any_element()
            }
            _ => message(cx, "This file cannot be played here."),
        };
        self.loaded = content;
        element
    }
}

/// Label/value metadata rows on a muted card.
fn rows_block(rows: &[(SharedString, SharedString)], cx: &Context<MediaPanel>) -> AnyElement {
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

/// A large placeholder tile with a centered glyph (audio artwork / no poster).
fn artwork(cx: &Context<MediaPanel>, icon: PikuIcon, height: f32) -> AnyElement {
    div()
        .w_full()
        .h(px(height))
        .flex()
        .items_center()
        .justify_center()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            Icon::new(icon)
                .size(px(56.))
                .text_color(cx.theme().muted_foreground),
        )
        .into_any_element()
}

fn message(cx: &Context<MediaPanel>, text: &str) -> AnyElement {
    div()
        .w_full()
        .h(px(160.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(text.to_string()),
        )
        .into_any_element()
}

impl Render for MediaPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("MediaPanel");
        div()
            .id("piku-media-panel")
            .track_focus(&self.focus_handle)
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .child(self.render_content(cx))
    }
}

impl Focusable for MediaPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for MediaPanel {}

impl Panel for MediaPanel {
    fn panel_name(&self) -> &'static str {
        Self::PANEL_NAME
    }

    fn tab_name(&self, _: &App) -> Option<SharedString> {
        Some(self.file_name())
    }

    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_video = matches!(self.loaded.as_deref(), Some(PreviewContent::Video { .. }))
            || self.path.as_ref().is_some_and(|p| {
                matches!(kind_for_path(p), crate::preview::PreviewKind::VideoMeta)
            });
        let icon = if is_video {
            PikuIcon::Film
        } else {
            PikuIcon::Music
        };
        h_flex()
            .gap_1()
            .items_center()
            .child(
                Icon::new(icon)
                    .size(px(14.))
                    .text_color(cx.theme().muted_foreground),
            )
            .child(self.file_name())
    }

    fn closable(&self, _: &App) -> bool {
        true
    }

    fn zoomable(&self, _: &App) -> Option<PanelControl> {
        None
    }

    fn dump(&self, _: &App) -> PanelState {
        let mut state = PanelState::new(self);
        if let Some(path) = &self.path
            && let Ok(value) = serde_json::to_value(path)
        {
            state.info = PanelInfo::panel(value);
        }
        state
    }
}
