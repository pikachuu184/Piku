//! The wire between the UI and the backend services.
//!
//! Two shapes, and only two:
//!
//! * [`BackendTask<T>`] — one request, one answer (`OpenFolder`, `RenameEntry`).
//! * [`BackendStream<T, S>`] — one request, many items, then a terminal
//!   summary (`ReadDirectory`, `MoveEntries`, `CalculateFolderSize`).
//!
//! Both cancel when dropped. That is the whole cancellation story: the four
//! hand-rolled `Arc<AtomicBool>` flags and four `generation: u64` counters in
//! the current UI collapse into holding (or not holding) an [`Inflight`].

// Stage 7 consumed most of this: BackendTask, BackendStream, Inflight, Cancel,
// StreamItem, both sinks and both channel constructors are live. What remains
// is waiting on specific work — `Progress` and `StreamItem::Progress` for the
// transfer engine, `send`/`finish_async` for a producer that is async rather
// than blocking.
//
// `expect` rather than `allow`: once the last item is constructed, this
// attribute itself starts erroring, which is the reminder to delete it.
#![expect(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::backend::error::BackendError;
use crate::backend::runtime::BackendRuntime;

/// The "stop now" signal, shaped as an error so a worker can write
/// `cancel.check()?` at a loop head.
///
/// Each service's error enum converts from this, which is what keeps the
/// cancellation path a compiler-checked variant rather than a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// The consumer-side cancellation state a worker observes.
///
/// Two tokens, because a worker must stop for two independent reasons: its
/// request was superseded, or the whole runtime is draining. Bundling them
/// means a provider takes one parameter and cannot forget the second reason.
///
/// Providers take `&Cancel` rather than a sink so they stay independent of
/// which response shape ([`BackendTask`] or [`BackendStream`]) called them,
/// and so a test can construct one without starting a runtime.
#[derive(Clone, Debug)]
pub struct Cancel {
    req: CancellationToken,
    shutdown: CancellationToken,
}

impl Cancel {
    pub fn is_cancelled(&self) -> bool {
        self.req.is_cancelled() || self.shutdown.is_cancelled()
    }

    /// `Err(Cancelled)` once either token fires. Call at the head of any loop
    /// that can run longer than a frame.
    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
    }

    /// A handle that never cancels.
    ///
    /// For the few callers that genuinely have no request to be superseded by:
    /// tests, and work initiated outside a `BackendTask` (an explicit
    /// user-driven screenshot, say). Reaching for this inside a service is a
    /// smell — it means the request's own token did not get threaded through.
    pub fn never() -> Self {
        Self {
            req: CancellationToken::new(),
            shutdown: CancellationToken::new(),
        }
    }

    /// A handle that is already cancelled, for asserting a worker stops.
    #[cfg(test)]
    pub fn already() -> Self {
        let req = CancellationToken::new();
        req.cancel();
        Self {
            req,
            shutdown: CancellationToken::new(),
        }
    }
}

/// Correlates a UI dispatch with its backend spans and its result.
///
/// Recorded on the operation's root span as `req`, echoed by the dispatch
/// layer when the result is applied, so a log can be read end to end.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RequestId(pub u64);

impl RequestId {
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Progress for operations that know their totals.
///
/// Totals are zero until a scan establishes them; consumers should treat
/// `total_* == 0` as "indeterminate" rather than "complete".
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Progress {
    pub done_bytes: u64,
    pub total_bytes: u64,
    pub done_items: u64,
    pub total_items: u64,
}

impl Progress {
    /// Completion in `0.0..=100.0`, byte-weighted when byte totals are known.
    /// Mirrors `Job::percent` so the status bar reads the same either way.
    pub fn percent(&self) -> f32 {
        if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64 * 100.0) as f32
        } else if self.total_items > 0 {
            (self.done_items as f64 / self.total_items as f64 * 100.0) as f32
        } else {
            0.0
        }
    }
}

/// One item off a [`BackendStream`].
///
/// Two type parameters so each service keeps its own payload *and* its own
/// terminal summary. A single shared response enum would force every call site
/// to match arms it knows cannot occur — exactly what `#![deny(clippy::panic)]`
/// exists to prevent.
#[derive(Debug)]
pub enum StreamItem<T, S = ()> {
    /// A batch of results. Batched rather than per-item so a 50 000-entry
    /// listing costs tens of channel sends, not 50 000.
    Batch(Vec<T>),
    /// Updated totals for a long-running operation.
    Progress(Progress),
    /// Terminal. Exactly one of these arrives, and nothing follows it.
    Done(Result<S, BackendError>),
}

impl<T, S> StreamItem<T, S> {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done(_))
    }
}

/// A cancellation handle for work in flight.
///
/// Dropping it cancels the request. Panels hold one per concurrent operation
/// (`dir_req`, `search_req`); assigning a new one drops the old, which both
/// supersedes the result *and* actually stops the worker — the current
/// generation counters only do the former.
#[derive(Debug)]
pub struct Inflight {
    id: RequestId,
    token: CancellationToken,
}

impl Inflight {
    pub fn id(&self) -> RequestId {
        self.id
    }

    /// Cancel now rather than at drop.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl Drop for Inflight {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// A one-shot request. Cancels on drop.
#[derive(Debug)]
pub struct BackendTask<T> {
    id: RequestId,
    token: CancellationToken,
    /// `None` once joined. Taken rather than moved out because `BackendTask`
    /// implements `Drop` and so cannot be destructured.
    rx: Option<oneshot::Receiver<T>>,
}

impl<T> BackendTask<T> {
    pub fn id(&self) -> RequestId {
        self.id
    }

    /// A handle that cancels this task when dropped.
    ///
    /// Mirrors [`BackendStream::inflight`]. The dispatch layer consumes the
    /// task by `join`ing it, so a view that wants to supersede a one-shot
    /// request has to hold the leash separately — this is that leash.
    pub fn inflight(&self) -> Inflight {
        Inflight {
            id: self.id,
            token: self.token.clone(),
        }
    }

    /// Await the answer.
    ///
    /// `Err(BackendError::ShuttingDown)` means the worker went away without
    /// replying — the runtime is draining, or the task was cancelled.
    ///
    /// `self` stays alive across the await on purpose: the cancellation token
    /// must outlive the wait, or joining would cancel the very work it is
    /// waiting for.
    pub async fn join(mut self) -> Result<T, BackendError> {
        let Some(rx) = self.rx.take() else {
            return Err(BackendError::ShuttingDown);
        };
        rx.await.map_err(|_| BackendError::ShuttingDown)
    }
}

impl<T> Drop for BackendTask<T> {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// A streaming request. Cancels on drop.
#[derive(Debug)]
pub struct BackendStream<T, S = ()> {
    id: RequestId,
    token: CancellationToken,
    rx: mpsc::Receiver<StreamItem<T, S>>,
}

impl<T, S> BackendStream<T, S> {
    pub fn id(&self) -> RequestId {
        self.id
    }

    /// A handle that cancels this stream when dropped.
    ///
    /// Cloning the token rather than the stream keeps ownership clear: the
    /// consumer drains, the panel holds the leash.
    pub fn inflight(&self) -> Inflight {
        Inflight {
            id: self.id,
            token: self.token.clone(),
        }
    }

    /// Next item, or `None` once the producer has finished and the channel is
    /// drained.
    pub async fn next(&mut self) -> Option<StreamItem<T, S>> {
        self.rx.recv().await
    }
}

impl<T, S> Drop for BackendStream<T, S> {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

/// The producer half of a [`BackendStream`], handed to the worker.
///
/// Sends are bounded, so a UI that stops draining stops the producer — which
/// is correct: a superseded pane should stop burning I/O rather than filling
/// memory with results nobody will render.
pub struct StreamSink<T, S = ()> {
    tx: mpsc::Sender<StreamItem<T, S>>,
    token: CancellationToken,
}

/// Guard against the one way to misuse this type.
///
/// `mpsc::Sender::blocking_send` is *designed* for the synchronous body of a
/// `spawn_blocking` worker — that is not the hazard. The hazard is calling it
/// from an **async task**, where it panics with "Cannot block the current
/// thread from within a runtime".
///
/// That panic would be silent in production: the task unwinds, the sink drops,
/// the consumer sees the channel close, and the view waits forever on a `Done`
/// that never arrives. Rather than guess at Tokio's internal state, we assert
/// against a marker that [`crate::backend::runtime::BackendRuntime::blocking`]
/// sets for exactly the duration of the closure it runs — so the check is
/// precise about *our* sanctioned blocking context rather than about Tokio's.
#[inline]
fn debug_assert_blocking_context(method: &str) {
    debug_assert!(
        crate::backend::runtime::in_blocking_context(),
        "`{method}` was called outside `runtime.blocking(..)`; \
         use the async variant (`send` / `finish_async`) from async code"
    );
}

impl<T, S> StreamSink<T, S> {
    /// Whether the request has been cancelled or the consumer has gone away.
    /// Workers should check this at loop heads.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled() || self.tx.is_closed()
    }

    /// A [`Cancel`] observing both this request and runtime shutdown, for
    /// handing to work that should not know about sinks.
    pub fn cancel_handle(&self, rt: &BackendRuntime) -> Cancel {
        Cancel {
            req: self.token.clone(),
            shutdown: rt.shutdown_token().clone(),
        }
    }

    /// Send from a **blocking** context (inside `runtime.blocking(..)`).
    ///
    /// Returns `false` if the item could not be delivered, which always means
    /// "stop working" — either cancelled or the consumer dropped.
    pub fn send_blocking(&self, item: StreamItem<T, S>) -> bool {
        if self.is_cancelled() {
            return false;
        }
        debug_assert_blocking_context("send_blocking");
        self.tx.blocking_send(item).is_ok()
    }

    /// Send from an async context.
    pub async fn send(&self, item: StreamItem<T, S>) -> bool {
        if self.is_cancelled() {
            return false;
        }
        self.tx.send(item).await.is_ok()
    }

    /// Deliver the terminal item from a **blocking** context. Consumes the
    /// sink so nothing can follow it.
    ///
    /// Unlike the non-terminal sends this does not bail on cancellation — a
    /// cancelled request still deserves its `Done`, so the consumer can clear
    /// its loading state. It uses `try_send` first precisely because a
    /// cancelled-but-undrained stream has a full channel and blocking there
    /// would hold a worker past the shutdown deadline.
    pub fn finish_blocking(self, result: Result<S, BackendError>) {
        debug_assert_blocking_context("finish_blocking");
        let mut item = StreamItem::Done(result);
        match self.tx.try_send(item) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(returned)) => {
                item = returned;
                // The consumer is alive but behind; a bounded wait is correct.
                let _ = self.tx.blocking_send(item);
            }
            // Receiver gone: nobody to tell, and that is fine.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    /// Deliver the terminal item from an async context.
    pub async fn finish_async(self, result: Result<S, BackendError>) {
        let _ = self.tx.send(StreamItem::Done(result)).await;
    }
}

/// Create a one-shot request/response pair.
pub fn task_channel<T>() -> (TaskSink<T>, BackendTask<T>) {
    let id = RequestId::next();
    let token = CancellationToken::new();
    let (tx, rx) = oneshot::channel();
    (
        TaskSink {
            tx: Some(tx),
            token: token.clone(),
        },
        BackendTask {
            id,
            token,
            rx: Some(rx),
        },
    )
}

/// The producer half of a [`BackendTask`].
pub struct TaskSink<T> {
    tx: Option<oneshot::Sender<T>>,
    token: CancellationToken,
}

impl<T> TaskSink<T> {
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// A [`Cancel`] observing both this request and runtime shutdown, for
    /// handing to work that should not know about sinks.
    pub fn cancel_handle(&self, rt: &BackendRuntime) -> Cancel {
        Cancel {
            req: self.token.clone(),
            shutdown: rt.shutdown_token().clone(),
        }
    }

    /// Deliver the answer. Dropping the sink without calling this surfaces as
    /// `BackendError::ShuttingDown` at the consumer.
    pub fn finish(mut self, value: T) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(value);
        }
    }
}

/// Create a streaming request/response pair with the given queue depth.
///
/// `capacity` is in *batches*, not items. Small is right: it bounds memory
/// while still letting the producer run a batch ahead of the renderer.
pub fn stream_channel<T, S>(capacity: usize) -> (StreamSink<T, S>, BackendStream<T, S>) {
    let id = RequestId::next();
    let token = CancellationToken::new();
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        StreamSink {
            tx,
            token: token.clone(),
        },
        BackendStream { id, token, rx },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> &'static crate::backend::runtime::BackendRuntime {
        crate::backend::runtime::get().expect("runtime")
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        rt().handle().block_on(f)
    }

    #[test]
    fn request_ids_are_unique_and_increasing() {
        let a = RequestId::next();
        let b = RequestId::next();
        assert!(b > a);
    }

    #[test]
    fn progress_is_byte_weighted_when_bytes_are_known() {
        let p = Progress {
            done_bytes: 50,
            total_bytes: 200,
            done_items: 1,
            total_items: 2,
        };
        assert_eq!(p.percent(), 25.0);
    }

    #[test]
    fn progress_falls_back_to_items_then_zero() {
        let items = Progress {
            done_items: 3,
            total_items: 4,
            ..Default::default()
        };
        assert_eq!(items.percent(), 75.0);
        assert_eq!(Progress::default().percent(), 0.0);
    }

    #[test]
    fn a_task_delivers_its_value() {
        let (sink, task) = task_channel::<u32>();
        sink.finish(9);
        assert_eq!(block_on(task.join()).unwrap(), 9);
    }

    #[test]
    fn a_dropped_task_sink_reports_shutting_down() {
        let (sink, task) = task_channel::<u32>();
        drop(sink);
        assert!(matches!(
            block_on(task.join()),
            Err(BackendError::ShuttingDown)
        ));
    }

    #[test]
    fn a_stream_delivers_batches_then_done() {
        let (sink, mut stream) = stream_channel::<u32, usize>(4);
        // Produced the way a real service does: inside `runtime.blocking`,
        // which is the only context where the `*_blocking` senders are legal.
        rt().blocking(move || {
            sink.send_blocking(StreamItem::Batch(vec![1, 2, 3]));
            sink.finish_blocking(Ok(3));
        });
        let first = block_on(stream.next()).expect("batch");
        assert!(matches!(first, StreamItem::Batch(ref b) if b == &[1, 2, 3]));
        let last = block_on(stream.next()).expect("done");
        assert!(matches!(last, StreamItem::Done(Ok(3))));
        assert!(last.is_done());
    }

    #[test]
    fn the_terminal_item_is_delivered_even_when_the_stream_is_cancelled() {
        // A cancelled request still owes its consumer a `Done`, or the view
        // never clears its loading state. Regression guard for the earlier
        // `finish` that bailed on cancellation and could block forever.
        let (sink, mut stream) = stream_channel::<u32, usize>(1);
        stream.inflight().cancel();
        rt().blocking(move || {
            // Non-terminal sends correctly refuse once cancelled...
            assert!(!sink.send_blocking(StreamItem::Batch(vec![1])));
            // ...but the terminal one still lands.
            sink.finish_blocking(Err(BackendError::ShuttingDown));
        });
        let last = block_on(stream.next()).expect("done still delivered");
        assert!(matches!(last, StreamItem::Done(Err(_))));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "outside `runtime.blocking")]
    fn blocking_sends_from_the_wrong_context_are_caught_in_debug() {
        let (sink, _stream) = stream_channel::<u32, ()>(4);
        // A plain thread is not a sanctioned blocking context: in real code
        // this shape would be an async task, where `blocking_send` panics
        // deep inside Tokio and silently loses the terminal item.
        std::thread::spawn(move || {
            sink.send_blocking(StreamItem::Batch(vec![1]));
        })
        .join()
        .expect_err("the guard should have fired");
        panic!("`send_blocking` outside `runtime.blocking` was not caught");
    }

    #[test]
    fn dropping_the_stream_cancels_the_producer() {
        let (sink, stream) = stream_channel::<u32, ()>(1);
        assert!(!sink.is_cancelled());
        drop(stream);
        assert!(sink.is_cancelled());
        // And a send after cancellation refuses rather than blocking forever.
        assert!(!sink.send_blocking(StreamItem::Batch(vec![1])));
    }

    #[test]
    fn an_inflight_outlives_the_stream_and_still_cancels() {
        let (sink, stream) = stream_channel::<u32, ()>(1);
        let inflight = stream.inflight();
        assert_eq!(inflight.id(), stream.id());
        assert!(!inflight.is_cancelled());
        inflight.cancel();
        assert!(sink.is_cancelled());
    }

    #[test]
    fn dropping_an_inflight_cancels() {
        let (sink, _stream) = stream_channel::<u32, ()>(1);
        {
            let _leash = _stream.inflight();
        }
        assert!(sink.is_cancelled());
    }

    #[test]
    fn a_dropped_task_cancels_its_sink() {
        let (sink, task) = task_channel::<u32>();
        assert!(!sink.is_cancelled());
        drop(task);
        assert!(sink.is_cancelled());
    }

    #[test]
    fn a_task_inflight_cancels_the_sink_when_dropped() {
        let (sink, task) = task_channel::<u32>();
        let leash = task.inflight();
        assert_eq!(leash.id(), task.id());
        assert!(!sink.is_cancelled());
        drop(leash);
        assert!(
            sink.is_cancelled(),
            "dropping the leash must stop the worker, not merely discard its answer"
        );
    }

    #[test]
    fn a_cancel_handle_observes_the_request_token() {
        let (sink, task) = task_channel::<u32>();
        let cancel = sink.cancel_handle(rt());
        assert!(cancel.check().is_ok());
        drop(task);
        assert!(cancel.is_cancelled());
        assert_eq!(cancel.check(), Err(Cancelled));
    }

    #[test]
    fn a_stream_cancel_handle_observes_the_request_token() {
        let (sink, stream) = stream_channel::<u32, ()>(1);
        let cancel = sink.cancel_handle(rt());
        assert!(cancel.check().is_ok());
        stream.inflight().cancel();
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn the_test_cancel_constructors_do_what_they_say() {
        assert!(Cancel::never().check().is_ok());
        assert!(Cancel::already().is_cancelled());
    }

    #[test]
    fn joining_a_task_does_not_cancel_it_first() {
        // Regression guard: an earlier shape dropped `self` (and therefore the
        // token) before awaiting, cancelling the work it was waiting for.
        let (sink, task) = task_channel::<u32>();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if sink.is_cancelled() {
                return false;
            }
            sink.finish(1);
            true
        });
        let got = block_on(task.join());
        assert!(handle.join().unwrap(), "the sink was cancelled mid-flight");
        assert_eq!(got.unwrap(), 1);
    }
}
