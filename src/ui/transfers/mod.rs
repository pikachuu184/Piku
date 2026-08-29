//! The two transfer surfaces, and the decision surface between them.
//!
//! | Module | What it draws |
//! | --- | --- |
//! | [`waveform`] | The throughput bars, and the aggregate the popover shows |
//! | [`card`] | One job in full: ring, identity, waveform, numbers, failures |
//! | [`popover`] | The compact panel above the status bar |
//! | [`center`] | Every job, grouped by what it is doing |
//! | [`conflicts`] | The one place a `Decision` is built |
//!
//! # The division of labour
//!
//! Completion and activity are two signals with two widgets: a
//! [`ProgressCircle`](gpui_component::progress::ProgressCircle) fills toward 100%
//! and the waveform says whether bytes are moving *now*. One bar cannot say both —
//! a copy stalled at 60% and one flying through 60% draw the same bar.
//!
//! Nothing in here performs a file operation. Every button ends in a call on the
//! [`JobQueue`] store, which forwards to the engine; the worker never touches a
//! widget, and the widgets never touch the filesystem. Every path on screen has
//! been through `security::text` before it arrived, and the one piece of text a
//! user types here — a rename — goes through
//! [`FileName::new`](crate::backend::path::FileName::new) before it can become a
//! policy.

pub mod card;
pub mod center;
pub mod conflicts;
pub mod popover;
pub mod waveform;

use gpui::{App, Context};

use crate::services::jobs::JobQueue;
use crate::state::PikuState;

/// Run `f` against the global job store.
///
/// Cloning the entity out of the global first releases the immutable borrow that
/// `global(cx)` holds, so the `update` can borrow mutably — the same shape as
/// `media::transport::update_player`.
///
/// Shared by every module in here rather than copied into each: this is the single
/// seam through which the UI reaches the engine, and one copy of it is one place
/// to look when asking what the interface is allowed to do.
pub(crate) fn with_jobs(cx: &mut App, f: impl FnOnce(&mut JobQueue, &mut Context<JobQueue>)) {
    let jobs = PikuState::global(cx).jobs.clone();
    jobs.update(cx, f);
}
