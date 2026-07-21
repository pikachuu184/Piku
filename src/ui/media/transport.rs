//! View-agnostic audio transport widgets.
//!
//! Each function is generic over the host view `V` and only ever talks to the
//! global `AudioPlayer`, so the same controls render identically in the
//! inspector, the bottom bar, and a docked media panel. Every element id is
//! prefixed by the caller so several transports can live on screen at once
//! without colliding.
//!
//! Theme rules (same as the rest of the preview surface): only `cx.theme().*`
//! tokens, `.ghost().xsmall()` buttons with tooltips, `cx.theme().radius`
//! corners, no shadows or new hex values.

use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, ParentElement as _, Pixels, SharedString, Styled as _, canvas, div, fill, point,
    px, size,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::app::assets::PikuIcon;
use crate::services::audio_player::{AudioPlayer, TrackMeta};
use crate::state::PikuState;

/// Mutate the global player from within a view listener. Cloning the entity
/// handle first releases the immutable borrow that `global(cx)` holds, so the
/// subsequent `update` can borrow `cx` mutably.
fn update_player<V: 'static>(
    cx: &mut Context<V>,
    f: impl FnOnce(&mut AudioPlayer, &mut Context<AudioPlayer>),
) {
    let audio = PikuState::global(cx).audio.clone();
    audio.update(cx, f);
}

/// A live snapshot of the global player as it pertains to one track.
struct Snapshot {
    is_current: bool,
    is_playing: bool,
    pos_ms: u64,
    volume: f32,
    muted: bool,
}

fn snapshot<V: 'static>(path: &Path, cx: &Context<V>) -> Snapshot {
    let player = PikuState::global(cx).audio.read(cx);
    let is_current = player.is_current(path);
    Snapshot {
        is_current,
        is_playing: is_current && player.is_playing(),
        pos_ms: if is_current {
            player.position().as_millis().min(u128::from(u64::MAX)) as u64
        } else {
            0
        },
        volume: player.volume(),
        muted: player.is_muted(),
    }
}

/// Build now-playing metadata from tag rows, falling back to the file name.
pub fn track_meta_from(
    path: &Path,
    rows: &[(SharedString, SharedString)],
    duration_ms: u64,
) -> TrackMeta {
    let find = |label: &str| {
        rows.iter().find(|(k, _)| k.as_ref() == label).map(|(_, v)| v.clone())
    };
    let title = find("Title").unwrap_or_else(|| {
        path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string().into()
    });
    TrackMeta { title, artist: find("Artist"), duration_ms }
}

/// `mm:ss` clock label.
pub fn fmt_clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn id(prefix: &str, suffix: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{suffix}"))
}

/// Fraction (0..1) of `bounds`' width that window-x `x` falls at.
fn fraction_at(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
    let w = f32::from(bounds.size.width);
    if w <= 0.0 {
        return 0.0;
    }
    ((f32::from(x) - f32::from(bounds.origin.x)) / w).clamp(0.0, 1.0)
}

/// The full inspector/panel transport: interactive waveform + control row.
pub fn audio_transport<V: 'static>(
    prefix: &'static str,
    path: &Path,
    meta: TrackMeta,
    waveform: &[f32],
    duration_ms: u64,
    cx: &mut Context<V>,
) -> AnyElement {
    let snap = snapshot(path, cx);
    let dur = duration_ms.max(1);
    let progress = (snap.pos_ms as f32 / dur as f32).clamp(0.0, 1.0);

    v_flex()
        .w_full()
        .gap_2()
        .child(scrubber(waveform, progress, dur, 56.0, cx))
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(transport_buttons(prefix, path, meta, dur, cx))
                .child(volume_control(prefix, snap.volume, snap.muted, cx))
                .child(
                    div()
                        .ml_auto()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} / {}", fmt_clock(snap.pos_ms), fmt_clock(dur))),
                ),
        )
        .into_any_element()
}

/// Play/pause · back 10s · forward 10s · stop, wired to the global player.
pub fn transport_buttons<V: 'static>(
    prefix: &str,
    path: &Path,
    meta: TrackMeta,
    dur: u64,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let snap = snapshot(path, cx);
    let (is_current, is_playing) = (snap.is_current, snap.is_playing);
    let play_path = path.to_path_buf();
    let play_icon = if is_playing { PikuIcon::Pause } else { PikuIcon::Play };
    let play_tip = if is_playing {
        "Pause"
    } else if is_current {
        "Resume"
    } else {
        "Play"
    };

    h_flex()
        .items_center()
        .gap_1()
        .child(
            Button::new(id(prefix, "play"))
                .ghost()
                .xsmall()
                .icon(play_icon)
                .tooltip(play_tip)
                .on_click(cx.listener(move |_, _, _, cx| {
                    let path = play_path.clone();
                    let meta = meta.clone();
                    update_player(cx, |player, cx| player.play_or_toggle_meta(path, Some(meta), cx));
                })),
        )
        .child(
            Button::new(id(prefix, "back"))
                .ghost()
                .xsmall()
                .icon(PikuIcon::Rewind)
                .tooltip("Back 10s")
                .disabled(!is_current)
                .on_click(cx.listener(|_, _, _, cx| {
                    update_player(cx, |player, cx| {
                        let target = player.position().saturating_sub(Duration::from_secs(10));
                        player.seek(target, cx);
                    });
                })),
        )
        .child(
            Button::new(id(prefix, "fwd"))
                .ghost()
                .xsmall()
                .icon(PikuIcon::FastForward)
                .tooltip("Forward 10s")
                .disabled(!is_current)
                .on_click(cx.listener(move |_, _, _, cx| {
                    let limit = Duration::from_millis(dur.max(1));
                    update_player(cx, |player, cx| {
                        let target = (player.position() + Duration::from_secs(10)).min(limit);
                        player.seek(target, cx);
                    });
                })),
        )
        .child(
            Button::new(id(prefix, "stop"))
                .ghost()
                .xsmall()
                .icon(PikuIcon::Square)
                .tooltip("Stop")
                .disabled(!is_current)
                .on_click(cx.listener(|_, _, _, cx| {
                    update_player(cx, |player, cx| player.stop(cx));
                })),
        )
}

/// Mute toggle + a click/drag volume track.
pub fn volume_control<V: 'static>(
    prefix: &str,
    volume: f32,
    muted: bool,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let icon = if muted || volume <= 0.0 { PikuIcon::VolumeX } else { PikuIcon::Volume2 };
    let shown = if muted { 0.0 } else { volume };
    let fill_color = cx.theme().foreground;

    let cell: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let (store, down, mv) = (cell.clone(), cell.clone(), cell);

    h_flex()
        .items_center()
        .gap_1()
        .child(
            Button::new(id(prefix, "mute"))
                .ghost()
                .xsmall()
                .icon(icon)
                .tooltip(if muted { "Unmute" } else { "Mute" })
                .on_click(cx.listener(|_, _, _, cx| {
                    update_player(cx, |player, cx| player.toggle_mute(cx));
                })),
        )
        .child(
            div()
                .w(px(64.))
                .h(px(18.))
                .flex()
                .items_center()
                .cursor_pointer()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                        let frac = fraction_at(down.get(), event.position.x);
                        update_player(cx, |player, cx| player.set_volume(frac, cx));
                    }),
                )
                .on_mouse_move(cx.listener(move |_, event: &MouseMoveEvent, _, cx| {
                    if !event.dragging() {
                        return;
                    }
                    let frac = fraction_at(mv.get(), event.position.x);
                    update_player(cx, |player, cx| player.set_volume(frac, cx));
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
                                |_, _, _| (),
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

/// Interactive scrubber: draws the waveform (or a flat progress track when no
/// peaks were generated) filled up to `progress`, with a playhead; click or
/// drag anywhere on it to seek.
pub fn scrubber<V: 'static>(
    waveform: &[f32],
    progress: f32,
    dur: u64,
    height: f32,
    cx: &mut Context<V>,
) -> impl IntoElement {
    let peaks = waveform.to_vec();
    let played = cx.theme().foreground;
    let rest = cx.theme().border;

    let cell: Rc<Cell<Bounds<Pixels>>> = Rc::new(Cell::new(Bounds::default()));
    let (store, down, mv) = (cell.clone(), cell.clone(), cell);

    div()
        .relative()
        .w_full()
        .h(px(height))
        .rounded(cx.theme().radius)
        .bg(cx.theme().muted)
        .overflow_hidden()
        .cursor_pointer()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_, event: &MouseDownEvent, _, cx| {
                let frac = fraction_at(down.get(), event.position.x);
                let target = Duration::from_millis((frac * dur.max(1) as f32) as u64);
                update_player(cx, |player, cx| player.seek(target, cx));
            }),
        )
        .on_mouse_move(cx.listener(move |_, event: &MouseMoveEvent, _, cx| {
            if !event.dragging() {
                return;
            }
            let frac = fraction_at(mv.get(), event.position.x);
            let target = Duration::from_millis((frac * dur.max(1) as f32) as u64);
            update_player(cx, |player, cx| player.seek(target, cx));
        }))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    store.set(bounds);
                    let w = f32::from(bounds.size.width);
                    let h = f32::from(bounds.size.height);
                    let ox = f32::from(bounds.origin.x);
                    let oy = f32::from(bounds.origin.y);
                    if peaks.is_empty() {
                        // No decoded peaks (e.g. oversized file) — show a plain
                        // progress track so the scrubber is still usable.
                        let track_h = 4.0f32.min(h);
                        let y = oy + (h - track_h) / 2.0;
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(px(ox), px(y)),
                                size: size(px(w), px(track_h)),
                            },
                            rest,
                        ));
                        window.paint_quad(fill(
                            Bounds {
                                origin: point(px(ox), px(y)),
                                size: size(px(w * progress), px(track_h)),
                            },
                            played,
                        ));
                    } else {
                        let n = peaks.len();
                        let slot = (w / n as f32).max(1.0);
                        let bar_w = (slot - 2.0).max(1.0);
                        for (i, peak) in peaks.iter().enumerate() {
                            let bar_h = (peak * (h - 8.0)).max(2.0);
                            let x = ox + i as f32 * slot + 1.0;
                            let y = oy + (h - bar_h) / 2.0;
                            let color = if (i as f32 + 0.5) / n as f32 <= progress {
                                played
                            } else {
                                rest
                            };
                            window.paint_quad(fill(
                                Bounds {
                                    origin: point(px(x), px(y)),
                                    size: size(px(bar_w), px(bar_h)),
                                },
                                color,
                            ));
                        }
                    }
                    // Playhead.
                    let head_x = ox + w * progress;
                    window.paint_quad(fill(
                        Bounds {
                            origin: point(px(head_x - 1.0), px(oy)),
                            size: size(px(2.0), px(h)),
                        },
                        played,
                    ));
                },
            )
            .size_full(),
        )
}
