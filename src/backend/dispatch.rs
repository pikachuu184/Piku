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
use crate::backend::error::BackendError;
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
    /// `apply` receives a `Result`, and is called **exactly once** as long as
    /// the view is still alive — including when the backend goes away without
    /// replying. Swallowing that case is how a view ends up showing skeleton
    /// rows forever, so it is not swallowed here.
    ///
    /// `cx.notify()` is called for you after `apply` returns.
    fn backend_task<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, Result<T, BackendError>, &mut Context<V>) + 'static,
    );

    /// As [`backend_task`](Self::backend_task), but hands back the leash.
    ///
    /// Use this wherever a request can be superseded before it finishes — a
    /// selection change, a re-navigation. Store the [`Inflight`] in the view
    /// and assign over it: the old one drops, and the worker actually
    /// **stops**. That is the difference from a `generation: u64` counter,
    /// which only discards the answer after the work has already been done.
    ///
    /// `apply` still runs exactly once, including for the cancelled request —
    /// so check `BackendError::is_cancelled()` before raising a toast.
    #[must_use = "dropping the Inflight immediately cancels the request"]
    fn backend_task_cancellable<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, Result<T, BackendError>, &mut Context<V>) + 'static,
    ) -> Inflight;

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

/// The shared body of the two one-shot dispatchers.
///
/// Factored out rather than having one call the other, because the difference
/// between them is precisely who owns the cancellation token: creating an
/// [`Inflight`] and dropping it inside `backend_task` would cancel every
/// request the instant it was issued.
fn spawn_task_apply<V: 'static, T: Send + 'static>(
    cx: &mut Context<V>,
    task: BackendTask<T>,
    apply: impl FnOnce(&mut V, Result<T, BackendError>, &mut Context<V>) + 'static,
) {
    let id = task.id();
    tracing::debug!(target: "piku::dispatch", req = %id, "dispatched");

    cx.spawn(async move |this, cx| {
        let result = task.join().await;
        if let Err(error) = &result {
            tracing::debug!(target: "piku::dispatch", req = %id, %error, "failed");
        }
        let _ = this.update(cx, |view, cx| {
            apply(view, result, cx);
            cx.notify();
        });
    })
    .detach();
}

impl<V: 'static> BackendExt<V> for Context<'_, V> {
    fn backend_task<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, Result<T, BackendError>, &mut Context<V>) + 'static,
    ) {
        let task = make(PikuState::global(self).backend());
        spawn_task_apply(self, task, apply);
    }

    fn backend_task_cancellable<T: Send + 'static>(
        &mut self,
        make: impl FnOnce(&Backend) -> BackendTask<T>,
        apply: impl FnOnce(&mut V, Result<T, BackendError>, &mut Context<V>) + 'static,
    ) -> Inflight {
        let task = make(PikuState::global(self).backend());
        // Taken before the task moves into the future: afterwards the token is
        // reachable only from inside the spawned closure.
        let inflight = task.inflight();
        spawn_task_apply(self, task, apply);
        inflight
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
            // The consumer is promised exactly one terminal item. A producer
            // that dies without calling `finish` (a panic, a dropped sink, a
            // runtime drain) closes the channel silently, and without the
            // synthesized `Done` below the view would sit in its loading state
            // forever. `Flow::Stop` is the one case that needs no terminal:
            // the consumer asked to stop and has already moved on.
            let mut delivered_done = false;
            let mut view_alive = true;

            while let Some(item) = stream.next().await {
                let done = item.is_done();
                let stop = this
                    .update(cx, |view, cx| {
                        let flow = apply(view, item, cx);
                        cx.notify();
                        flow == Flow::Stop
                    })
                    // The view is gone; nothing left to deliver to.
                    .unwrap_or_else(|_| {
                        view_alive = false;
                        true
                    });
                if done {
                    delivered_done = true;
                    break;
                }
                if stop {
                    tracing::debug!(
                        target: "piku::dispatch", req = %id, superseded = true, "stopped"
                    );
                    // Superseded by the consumer's own choice — no terminal owed.
                    delivered_done = true;
                    break;
                }
            }

            if !delivered_done && view_alive {
                tracing::debug!(
                    target: "piku::dispatch", req = %id, "producer ended without a terminal item"
                );
                let _ = this.update(cx, |view, cx| {
                    apply(view, StreamItem::Done(Err(BackendError::ShuttingDown)), cx);
                    cx.notify();
                });
            }
        })
        .detach();

        inflight
    }
}
