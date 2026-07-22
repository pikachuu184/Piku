//! Persistent bottom playback bar.
//!
//! A slim entity that lives directly in the workspace shell (outside the dock
//! graph) and mirrors the single global `AudioPlayer`. It renders nothing while
//! no track is loaded, so it never steals vertical space; once audio is playing
//! it stays visible across every tab, split and dock change, and closing the
//! inspector preview never interrupts playback.

use gpui::{
    Context, IntoElement, ParentElement as _, Render, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::app::actions::OpenMediaPanel;
use crate::app::assets::PikuIcon;
use crate::state::PikuState;
use crate::ui::media::transport;

pub struct MediaBar {
    _audio: Subscription,
}

impl MediaBar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let audio = PikuState::global(cx).audio.clone();
        Self {
            _audio: cx.observe(&audio, |_, _, cx| cx.notify()),
        }
    }
}

impl Render for MediaBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Pull an owned snapshot, releasing the player borrow before the
        // transport helpers borrow `cx` mutably.
        let (path, meta, pos_ms, dur, volume, muted) = {
            let player = PikuState::global(cx).audio.read(cx);
            let Some(path) = player.current().map(|p| p.to_path_buf()) else {
                // Nothing loaded → occupy zero height.
                return div().into_any_element();
            };
            (
                path,
                player.meta().cloned().unwrap_or_default(),
                player.position().as_millis().min(u128::from(u64::MAX)) as u64,
                player.duration_ms(),
                player.volume(),
                player.is_muted(),
            )
        };
        let dur = dur.max(1);
        let progress = (pos_ms as f32 / dur as f32).clamp(0.0, 1.0);

        let title = meta.title.clone();
        let artist = meta.artist.clone();

        h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            // Artwork placeholder.
            .child(
                div()
                    .flex_none()
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .child(
                        Icon::new(PikuIcon::Music)
                            .size(px(15.))
                            .text_color(cx.theme().muted_foreground),
                    ),
            )
            // Title / artist.
            .child(
                v_flex()
                    .flex_none()
                    .w(px(160.))
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().foreground)
                            .truncate()
                            .child(title),
                    )
                    .when_some(artist, |el, artist| {
                        el.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .truncate()
                                .child(artist),
                        )
                    }),
            )
            .child(transport::transport_buttons(
                "media-bar",
                &path,
                meta,
                dur,
                cx,
            ))
            .child(div().flex_1().min_w(px(80.)).child(transport::scrubber(
                &[],
                progress,
                dur,
                10.0,
                cx,
            )))
            .child(
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "{} / {}",
                        transport::fmt_clock(pos_ms),
                        transport::fmt_clock(dur)
                    )),
            )
            .child(transport::volume_control("media-bar", volume, muted, cx))
            // Pop the current track out into a dockable media panel.
            .child(
                Button::new("media-bar-expand")
                    .ghost()
                    .xsmall()
                    .icon(PikuIcon::Maximize2)
                    .tooltip("Open in media panel")
                    .on_click(cx.listener(move |_, _, window, cx| {
                        window.dispatch_action(Box::new(OpenMediaPanel(path.clone())), cx);
                    })),
            )
            .into_any_element()
    }
}
