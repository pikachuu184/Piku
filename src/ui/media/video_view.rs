//! In-app video player view: a scaled frame surface with an auto-hiding control
//! bar, driven by [`VideoPlayer`]. All decoding happens off-thread in the
//! player; this view only blits the current frame and forwards control clicks.
//!
//! Theme rules match the rest of the media surface: `.ghost().xsmall()` buttons
//! with tooltips, `cx.theme().radius` corners, tokens only — no new hex, no
//! shadows. The video surface itself is intentionally dark (`muted`) so frames
//! read well.
//!
//! ## Frames own GPU memory, so this view has to release them
//!
//! Every decoded frame is a distinct `RenderImage`, and painting one interns its
//! pixels into the window's sprite atlas under a globally unique id. `RenderImage`
//! has no `Drop` impl and `ImageSource::Render` never releases anything, so the
//! *only* thing that frees that tile is [`Window::drop_image`]. A 960-wide frame
//! cannot pack twice into gpui's 1024² atlas texture, so without an explicit
//! release each painted frame permanently owned ~4 MiB of VRAM — roughly
//! 120 MiB/s at 30 fps, which is why playback degraded the longer it ran.
//!
//! So this view keeps the last two painted frames ([`VideoView::shown`] and
//! [`VideoView::retired`]) and releases a tile only once two further frames have
//! been painted over it, and an `on_release` hook frees the final two whenever the
//! view goes away. Releasing is always *safe* rather than merely tolerable:
//! `paint_image` re-interns on a miss, so an early release costs one re-upload.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, ImageSource,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    ParentElement as _, Pixels, Render, RenderImage, Styled as _, StyledImage as _, Window, canvas,
    div, fill, img, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::app::assets::PikuIcon;
use crate::backend::dispatch::BackendExt as _;
use crate::backend::error::BackendError;
use crate::services::video_player::VideoPlayer;
use crate::ui::media::transport::fmt_clock;

/// Idle time before the control bar fades while playing.
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);

/// How long to keep saying "Loading…" before a decoder complaint is worth
/// showing. ffmpeg can gripe about a stream and still recover — and the player
/// retries in software when hardware decoding produces nothing — so a message
/// shown instantly would flash on files that go on to play fine.
const ERROR_GRACE: Duration = Duration::from_millis(1500);

pub struct VideoView {
    focus_handle: FocusHandle,
    player: VideoPlayer,
    fullscreen: bool,
    controls_visible: bool,
    hide_at: Option<Instant>,
    /// The frame painted by the previous render pass.
    shown: Option<Arc<RenderImage>>,
    /// The frame painted the pass before that, still holding its atlas tile.
    /// Released once a third frame has been painted over it — see the module
    /// note; dropping a tile the in-flight scene still points at is what the
    /// delay avoids.
    retired: Option<Arc<RenderImage>>,
}

impl VideoView {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let view = Self {
            focus_handle: cx.focus_handle(),
            player: VideoPlayer::new(path.clone()),
            fullscreen: false,
            controls_visible: true,
            hide_at: None,
            shown: None,
            retired: None,
        };
        view.start_ticker(cx);
        // The last two frames still hold atlas tiles when the view goes away
        // (panel closed, another file opened). `Drop` cannot free them — it has
        // neither a `Window` nor an `App` — but a release hook does. It runs
        // during effect flushing, after the window has been handed back, so
        // `drop_image` reaches every window that painted the frame.
        cx.on_release(|this, cx| this.release_images(cx)).detach();
        // The player starts silent: opening the file and building the audio
        // decoder is blocking work, and this constructor runs inside `cx.new`
        // on the UI thread. The decoder is `Send`, so it is built on a worker
        // and attached here; `OutputStream` is `!Send`, which is why the sink
        // itself is created on this side.
        cx.spawn(async move |this, cx| {
            let track = cx
                .background_executor()
                .spawn(async move { crate::services::video_player::open_audio_track(&path) })
                .await;
            if let Some(track) = track {
                let _ = this.update(cx, |this, _| this.player.attach_audio(track));
            }
        })
        .detach();
        view
    }

    /// Free the atlas tiles of every frame this view still holds.
    ///
    /// Called from the release hook registered in [`VideoView::new`]. Without it
    /// the last two frames of every video ever opened stay resident for the life
    /// of the process.
    fn release_images(&mut self, cx: &mut App) {
        for image in [self.retired.take(), self.shown.take()]
            .into_iter()
            .flatten()
        {
            // `None`: no window is mid-update here, so every window that could
            // have painted the frame is reachable through `App`.
            cx.drop_image(image, None);
        }
    }

    /// Keep the view re-rendering so decoded frames appear and the clock/scrubber
    /// advance. Ticks fast while playing, slow while paused (still catches frames
    /// that arrive after a seek); exits when the entity is dropped.
    fn start_ticker(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                let playing = this.update(cx, |this, cx| {
                    // Auto-hide the controls after an idle period while playing.
                    if this.player.is_playing() {
                        if let Some(at) = this.hide_at
                            && Instant::now() >= at
                        {
                            this.controls_visible = false;
                            this.hide_at = None;
                        }
                    } else {
                        this.controls_visible = true;
                    }
                    cx.notify();
                    this.player.is_playing()
                });
                let Ok(playing) = playing else {
                    break; // entity dropped
                };
                let delay = if playing { 33 } else { 120 };
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
            }
        })
        .detach();
    }

    fn reveal_controls(&mut self) {
        self.controls_visible = true;
        self.hide_at = Some(Instant::now() + CONTROLS_HIDE_AFTER);
    }

    /// Advance the two-deep history of painted frames, freeing the atlas tile of
    /// whatever falls off the end.
    ///
    /// `next` is what this pass is about to paint. Nothing happens while it is
    /// the same image as last pass (a paused player, or a tick between frames),
    /// so a still image is never re-uploaded.
    ///
    /// Frames the player skipped past without ever handing here were never
    /// painted and hold no tile, so they need no release — dropping their `Arc`
    /// is enough.
    fn rotate_frame(&mut self, next: Option<&Arc<RenderImage>>, window: &mut Window) {
        let unchanged = match (self.shown.as_ref(), next) {
            (Some(shown), Some(next)) => Arc::ptr_eq(shown, next),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return;
        }
        if let Some(old) = self.retired.take() {
            // Two frames have been painted over this one, so no scene still
            // references its tile.
            let _ = window.drop_image(old);
        }
        self.retired = self.shown.take();
        self.shown = next.cloned();
    }

    fn surface(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let height = if self.fullscreen { 640.0 } else { 380.0 };
        let frame = self.player.current_frame();
        self.rotate_frame(frame.as_ref(), window);

        let body: AnyElement = match frame {
            Some(image) => img(ImageSource::Render(image))
                .max_w_full()
                .max_h_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            // Nothing decoded yet. After a grace period an ffmpeg complaint is
            // worth showing: a failed decode used to be indistinguishable from a
            // slow one, both sitting behind "Loading…" forever. The message is
            // sanitized where it is recorded — it is subprocess output shaped by
            // an untrusted file.
            None => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(match self.player.error() {
                    Some(message) if self.player.startup_elapsed() >= ERROR_GRACE => message,
                    _ => "Loading…".to_string(),
                })
                .into_any_element(),
        };

        div()
            .id("video-surface")
            .relative()
            .w_full()
            .h(px(height))
            .rounded(cx.theme().radius)
            .bg(cx.theme().muted)
            .overflow_hidden()
            // Reveal controls on any pointer movement over the surface.
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, cx| {
                this.reveal_controls();
                cx.notify();
            }))
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(body),
            )
            .when(self.controls_visible, |el| {
                el.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .p_2()
                        .child(self.controls(cx)),
                )
            })
            .into_any_element()
    }

    fn controls(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let playing = self.player.is_playing();
        let pos = self.player.position_ms();
        let dur = self.player.duration_ms();
        let volume = self.player.volume();
        let muted = self.player.is_muted();
        let speed = self.player.speed();

        let play_icon = if playing {
            PikuIcon::Pause
        } else {
            PikuIcon::Play
        };

        v_flex()
            .w_full()
            .gap_1()
            .p_1()
            .rounded(cx.theme().radius)
            .bg(cx.theme().background.opacity(0.7))
            .child(self.timeline(pos, dur, cx))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(
                        Button::new("vv-play")
                            .ghost()
                            .xsmall()
                            .icon(play_icon)
                            .tooltip(if playing { "Pause" } else { "Play" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.player.toggle();
                                this.reveal_controls();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("vv-step-back")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::StepBack)
                            .tooltip("Previous frame")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.player.step(-1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("vv-step-fwd")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::StepForward)
                            .tooltip("Next frame")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.player.step(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("vv-back10")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::Rewind)
                            .tooltip("Back 10s")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let target = this.player.position_ms().saturating_sub(10_000);
                                this.player.seek(target);
                                this.reveal_controls();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("vv-fwd10")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::FastForward)
                            .tooltip("Forward 10s")
                            .on_click(cx.listener(|this, _, _, cx| {
                                let target = this.player.position_ms().saturating_add(10_000);
                                this.player.seek(target);
                                this.reveal_controls();
                                cx.notify();
                            })),
                    )
                    .child(self.volume_control(volume, muted, cx))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} / {}", fmt_clock(pos), fmt_clock(dur))),
                    )
                    .child(
                        Button::new("vv-speed")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::Gauge)
                            .label(format!("{}x", trim_speed(speed)))
                            .tooltip("Playback speed")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.player.cycle_speed();
                                this.reveal_controls();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("vv-shot")
                            .ghost()
                            .xsmall()
                            .icon(PikuIcon::Camera)
                            .tooltip("Save a screenshot")
                            .on_click(cx.listener(|this, _, window, cx| {
                                // ffmpeg takes hundreds of milliseconds to
                                // spawn and grab a frame; running it here froze
                                // the window on every screenshot.
                                let path = this.player.path().to_path_buf();
                                let at_ms = this.player.position_ms();
                                cx.backend_task_in(
                                    window,
                                    move |backend| backend.preview().save_frame(path, at_ms),
                                    |_, result, window, cx| {
                                        let toast = match result {
                                            Ok(dest) => crate::ui::toast::info(format!(
                                                "Saved {}",
                                                dest.display()
                                            )),
                                            Err(error) => crate::ui::toast::error(
                                                BackendError::from(error).user_message(),
                                            ),
                                        };
                                        window.push_notification(toast, cx);
                                    },
                                );
                            })),
                    )
                    .child(
                        Button::new("vv-full")
                            .ghost()
                            .xsmall()
                            .icon(if self.fullscreen {
                                PikuIcon::Minimize
                            } else {
                                PikuIcon::Maximize
                            })
                            .tooltip(if self.fullscreen { "Shrink" } else { "Expand" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.fullscreen = !this.fullscreen;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }

    /// Click/drag seek bar drawn over a plain progress track.
    fn timeline(&self, pos: u64, dur: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let progress = if dur > 0 {
            (pos as f32 / dur as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let played = cx.theme().foreground;
        let rest = cx.theme().muted_foreground;

        let cell: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
        let (store, down, mv) = (cell.clone(), cell.clone(), cell);

        div()
            .relative()
            .w_full()
            .h(px(12.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    let frac = fraction_at(down.get(), event.position.x);
                    this.player.seek((frac * dur.max(1) as f32) as u64);
                    this.reveal_controls();
                    cx.notify();
                }),
            )
            .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                if !event.dragging() {
                    return;
                }
                let frac = fraction_at(mv.get(), event.position.x);
                // `scrub`, not `seek`: a drag fires a move per frame, and each
                // one used to tear down and respawn both ffmpeg processes. The
                // clock and the bar still follow the pointer immediately; only
                // the decoder restart is coalesced.
                this.player.scrub((frac * dur.max(1) as f32) as u64);
                cx.notify();
            }))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(4.))
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .overflow_hidden()
                    .child(
                        canvas(
                            move |_, _, _| (),
                            move |bounds, _, window, _| {
                                store.set(bounds);
                                let w = f32::from(bounds.size.width);
                                window.paint_quad(fill(bounds, rest));
                                window.paint_quad(fill(
                                    Bounds {
                                        origin: bounds.origin,
                                        size: size(px(w * progress), bounds.size.height),
                                    },
                                    played,
                                ));
                            },
                        )
                        .size_full(),
                    ),
            )
    }

    fn volume_control(&self, volume: f32, muted: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = if muted || volume <= 0.0 {
            PikuIcon::VolumeX
        } else {
            PikuIcon::Volume2
        };
        let shown = if muted { 0.0 } else { volume };
        let fill_color = cx.theme().foreground;

        let cell: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
        let (store, down, mv) = (cell.clone(), cell.clone(), cell);

        h_flex()
            .items_center()
            .gap_1()
            .child(
                Button::new("vv-mute")
                    .ghost()
                    .xsmall()
                    .icon(icon)
                    .tooltip(if muted { "Unmute" } else { "Mute" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.player.toggle_mute();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .w(px(56.))
                    .h(px(16.))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            let frac = fraction_at(down.get(), event.position.x);
                            this.player.set_volume(frac);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(move |this, event: &MouseMoveEvent, _, cx| {
                        if !event.dragging() {
                            return;
                        }
                        let frac = fraction_at(mv.get(), event.position.x);
                        this.player.set_volume(frac);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .h(px(4.))
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .overflow_hidden()
                            .child(
                                canvas(
                                    move |_, _, _| (),
                                    move |bounds, _, window, _| {
                                        store.set(bounds);
                                        let fw = f32::from(bounds.size.width) * shown;
                                        window.paint_quad(fill(
                                            Bounds {
                                                origin: bounds.origin,
                                                size: size(px(fw.max(0.0)), bounds.size.height),
                                            },
                                            fill_color,
                                        ));
                                    },
                                )
                                .size_full(),
                            ),
                    ),
            )
    }
}

/// Fraction (0..1) of `bounds`' width that window-x `x` falls at.
fn fraction_at(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
    let w = f32::from(bounds.size.width);
    if w <= 0.0 {
        return 0.0;
    }
    ((f32::from(x) - f32::from(bounds.origin.x)) / w).clamp(0.0, 1.0)
}

/// Compact speed label (`1`, `1.5`, `0.5`) without a trailing `.0`.
fn trim_speed(speed: f32) -> String {
    if (speed - speed.round()).abs() < 0.01 {
        format!("{}", speed.round() as i32)
    } else {
        format!("{speed}")
    }
}

impl Render for VideoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("VideoView");
        div()
            .id("piku-video-view")
            .track_focus(&self.focus_handle)
            .w_full()
            .child(self.surface(window, cx))
    }
}

impl Focusable for VideoView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for VideoView {}
