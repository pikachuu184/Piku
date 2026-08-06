//! In-app video player view: a scaled frame surface with an auto-hiding control
//! bar, driven by [`VideoPlayer`]. All decoding happens off-thread in the
//! player; this view only blits the current frame and forwards control clicks.
//!
//! Theme rules match the rest of the media surface: `.ghost().xsmall()` buttons
//! with tooltips, `cx.theme().radius` corners, tokens only — no new hex, no
//! shadows. The video surface itself is intentionally dark (`muted`) so frames
//! read well.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Context, EventEmitter, FocusHandle, Focusable, ImageSource,
    InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit,
    ParentElement as _, Pixels, Render, Styled as _, StyledImage as _, Window, canvas, div, fill,
    img, prelude::FluentBuilder as _, px, size,
};
use gpui_component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::app::assets::PikuIcon;
use crate::services::video_player::VideoPlayer;
use crate::ui::media::transport::fmt_clock;

/// Idle time before the control bar fades while playing.
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);

pub struct VideoView {
    focus_handle: FocusHandle,
    player: VideoPlayer,
    fullscreen: bool,
    controls_visible: bool,
    hide_at: Option<Instant>,
}

impl VideoView {
    pub fn new(path: PathBuf, cx: &mut Context<Self>) -> Self {
        let view = Self {
            focus_handle: cx.focus_handle(),
            player: VideoPlayer::new(path),
            fullscreen: false,
            controls_visible: true,
            hide_at: None,
        };
        view.start_ticker(cx);
        view
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

    fn surface(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let height = if self.fullscreen { 640.0 } else { 380.0 };
        let frame = self.player.current_frame();

        let body: AnyElement = match frame {
            Some(image) => img(ImageSource::Render(image))
                .max_w_full()
                .max_h_full()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            None => div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Loading…")
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
                                if let Some(dest) = this.player.screenshot() {
                                    window.push_notification(
                                        crate::ui::toast::info(format!("Saved {}", dest.display())),
                                        cx,
                                    );
                                }
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
                this.player.seek((frac * dur.max(1) as f32) as u64);
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _pass = crate::app::diagnostics::enter_render("VideoView");
        div()
            .id("piku-video-view")
            .track_focus(&self.focus_handle)
            .w_full()
            .child(self.surface(cx))
    }
}

impl Focusable for VideoView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<()> for VideoView {}
