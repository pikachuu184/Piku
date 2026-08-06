//! The gpui bridge. **The only file under `backend/` that imports gpui.**
//!
//! Every backend call from the UI used to be written out longhand:
//!
//! ```ignore
//! let task = cx.background_executor().spawn(async move { blocking_call() });
//! cx.spawn(async move |this, cx| {
//!     let result = task.await;
//!     let _ = this.update(cx, |this, cx| {
//!         if this.generation != generation { return; }   // easy to forget
//!         /* apply */
//!         cx.notify();                                   // also easy to forget
//!     });
//! }).detach();
//! ```
//!
//! That shape appears about forty times in the tree, and each copy re-invents
//! staleness handling and cancellation. [`BackendExt`] collapses it to the two
//! lines that actually differ: which command to issue, and what to do with the
//! answer. `notify` is automatic, and cancellation is ownership — hold the
//! returned [`Inflight`] and the work lives, drop it and the worker stops.

use gpui::{Context, Window};

use crate::backend::Backend;
use crate::backend::protocol::{BackendStream, BackendTask, Inflight, StreamItem};
use crate::state::PikuState;

/// What a stream consumer wants to happen next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[expect(dead_code, reason = "wired up by the streaming listing in Stage 3")]
pub enum Flow {
    /// Keep draining.
    Continue,
    /// Stop draining and cancel the producer. Return this when the request has
    /// been superseded.
    Stop,
}

/// Dispatch backend commands from a gpui view.
pub trait BackendExt<V: 'static> {
    /// Issue a one-shot command and apply its result on the UI thread.
    ///
    /// `cx.notify()` is called for you after `apply` returns.
    fn backend_task<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
    );

    /// As [`backend_task`](Self::backend_task), but `apply` also receives the
    /// `Window` — needed for toasts and focus changes.
    #[expect(
        dead_code,
        reason = "wired up by the File/Transfer services in Stage 5"
    )]
    fn backend_task_in<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, T, &mut Window, &mut Context<V>) + 'static,
    );

    /// Issue a streaming command, applying each item as it arrives.
    ///
    /// Draining stops on `Done`, on [`Flow::Stop`], or when the view goes
    /// away. The returned [`Inflight`] cancels the producer when dropped, so
    /// storing it in the view is what ties the request's lifetime to the
    /// view's — and replacing it is what supersedes an older request.
    #[must_use = "dropping the Inflight immediately cancels the request"]
    #[expect(dead_code, reason = "wired up by the streaming listing in Stage 3")]
    fn backend_stream<T: Send + 'static, S: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendStream<T, S>,
        apply: impl FnMut(&mut V, StreamItem<T, S>, &mut Context<V>) -> Flow + 'static,
    ) -> Inflight;
}

impl<V: 'static> BackendExt<V> for Context<'_, V> {
    fn backend_task<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
    ) {
        let task = make(PikuState::global(self).backend());
        let id = task.id();
        tracing::debug!(target: "piku::dispatch", req = %id, "dispatched");

        self.spawn(async move |this, cx| {
            let Ok(value) = task.join().await else {
                tracing::debug!(target: "piku::dispatch", req = %id, "no result");
                return;
            };
            let _ = this.update(cx, |view, cx| {
                apply(view, value, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn backend_task_in<T: Send + 'static>(
        &mut self,
        window: &mut Window,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, T, &mut Window, &mut Context<V>) + 'static,
    ) {
        let task = make(PikuState::global(self).backend());
        let id = task.id();
        tracing::debug!(target: "piku::dispatch", req = %id, "dispatched");

        self.spawn_in(window, async move |this, cx| {
            let Ok(value) = task.join().await else {
                tracing::debug!(target: "piku::dispatch", req = %id, "no result");
                return;
            };
            let _ = this.update_in(cx, |view, window, cx| {
                apply(view, value, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn backend_stream<T: Send + 'static, S: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendStream<T, S>,
        mut apply: impl FnMut(&mut V, StreamItem<T, S>, &mut Context<V>) -> Flow + 'static,
    ) -> Inflight {
        let mut stream = make(PikuState::global(self).backend());
        let id = stream.id();
        let inflight = stream.inflight();
        tracing::debug!(target: "piku::dispatch", req = %id, "dispatched (stream)");

        self.spawn(async move |this, cx| {
            while let Some(item) = stream.next().await {
                let done = item.is_done();
                let stop = this
                    .update(cx, |view, cx| {
                        let flow = apply(view, item, cx);
                        cx.notify();
                        flow == Flow::Stop
                    })
                    // The view is gone; nothing left to deliver to.
                    .unwrap_or(true);
                if stop {
                    tracing::debug!(target: "piku::dispatch", req = %id, superseded = true, "stopped");
                    break;
                }
                if done {
                    break;
                }
            }
        })
        .detach();

        inflight
    }
}
