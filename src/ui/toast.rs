//! Narrow toast constructors. gpui-component's `Notification` defaults to
//! `w_112()` (448px), which reads as a banner; every PIKU toast goes through
//! these helpers so the width stays consistent app-wide.

use gpui::{SharedString, Styled as _, px};
use gpui_component::notification::Notification;

/// One width for every toast; long messages wrap inside the card.
const TOAST_WIDTH: f32 = 320.;

pub fn success(message: impl Into<SharedString>) -> Notification {
    Notification::success(message).w(px(TOAST_WIDTH))
}

pub fn info(message: impl Into<SharedString>) -> Notification {
    Notification::info(message).w(px(TOAST_WIDTH))
}

pub fn error(message: impl Into<SharedString>) -> Notification {
    Notification::error(message).w(px(TOAST_WIDTH))
}
