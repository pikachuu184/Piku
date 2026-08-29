//! The post-transfer seam: what wants to know that bytes landed.
//!
//! # Why a seam rather than a step in the copy
//!
//! Everything that would naturally be bolted onto the end of a transfer —
//! recording the job in SQLite, indexing the new text, hashing for content
//! identity, generating thumbnails, extracting document text — shares one
//! property: it is not the copy, and it is slower than the copy. Bolted on, a
//! 40 000-file paste ends with a 40 000-file BLAKE3 pass and a Tantivy commit
//! while the user is waiting for the progress bar to disappear.
//!
//! So they go behind a queue instead, and the queue has three properties that
//! are the whole point of the module:
//!
//! * **It never blocks the publisher.** [`Pipeline::publish`] is a `try_send`
//!   that either takes the payload or drops it. A copy's last act is a
//!   non-blocking channel write, whatever the sinks are doing.
//! * **It is one worker deep.** Sinks run one at a time, one completion at a
//!   time, on a single blocking thread. A transfer subsystem allowed four
//!   concurrent copies and an unbounded indexer is a transfer subsystem that
//!   spends its afternoon indexing.
//! * **It carries paths, never bytes.** There is nowhere in [`TransferCompleted`]
//!   to put a digest or a buffer, which is what makes hashing *conditional*:
//!   a sink that needs content identity computes it for the files it actually
//!   cares about, and one that does not never pays for it.
//!
//! # Why the paths are hints
//!
//! A sink runs after the fact, so every path it receives describes a filesystem
//! that has already moved on — the user can delete the pasted folder in the
//! second between the copy finishing and the indexer waking. Publishing
//! *exactly* which leaf files were written would therefore buy nothing, and cost
//! a `Vec<PathBuf>` push per file in the copy loop. The payload carries the
//! top-level shape instead, and a sink stats before it trusts.
//!
//! That also means a sink must re-authorize. Nothing here is a
//! [`ValidatedPath`](crate::backend::path::ValidatedPath): these paths were
//! authorized against the policy that was live when the job was approved, and a
//! sink walking them later is a fresh filesystem operation that needs a fresh
//! decision.
//!
//! # Status
//!
//! No sink is implemented at this stage, by design — see the plan's "out of
//! scope". [`Pipeline::inert`] is the shape that has none: no channel, no task,
//! and [`publish`](Pipeline::publish) compiles down to a null check. A copy stays
//! a copy until something is actually registered here.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::backend::runtime::BackendRuntime;
use crate::backend::services::transfer::job::{JobId, TransferKind, TransferSummary};

/// Completions buffered before the queue starts dropping them.
///
/// Deliberately small. The alternative — an unbounded queue — trades a bounded
/// memory cost for an unbounded one and still loses everything on a crash, so it
/// is not a durability mechanism, only a slower way to notice the same problem.
/// A sink that must not miss a completion owns that itself: the journal is the
/// sink's, not the channel's.
const QUEUE_DEPTH: usize = 64;

/// How the job ended, as far as a sink needs to care.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Ending {
    /// Every item the plan named was handled.
    Complete,
    /// Cancelled, failed, or finished with skips — so what is on disk is a
    /// subset of what the plan described.
    Partial,
}

/// What a finished job hands to the sinks.
///
/// Top-level shape only, and every path in it is a hint; see the module header
/// on both.
#[derive(Clone, Debug)]
pub struct TransferCompleted {
    pub id: JobId,
    pub kind: TransferKind,
    pub ending: Ending,
    /// The approved top-level sources, resolved, in the spelling the pass
    /// walked. Gone from disk by now for a move or a permanent delete.
    pub sources: Vec<PathBuf>,
    /// The approved destination directory. `None` for a permanent delete, which
    /// has nowhere to put anything.
    pub destination: Option<PathBuf>,
    /// The top-level destinations the scan planned, one per source. A `KeepBoth`
    /// resolution renames one of these under the sink's feet, which is the
    /// sharpest reason the whole list is hints.
    pub targets: Vec<PathBuf>,
    pub summary: TransferSummary,
}

impl TransferCompleted {
    /// Whether anything was created. `false` for a permanent delete, and for a
    /// copy that was cancelled before its first byte.
    pub fn wrote_anything(&self) -> bool {
        self.destination.is_some() && self.summary.items > 0
    }

    /// Whether anything was destroyed — a move's sources, or a delete's.
    pub fn removed_anything(&self) -> bool {
        self.kind.destroys_source() && self.summary.items > 0
    }
}

/// Something that wants to know a transfer finished.
///
/// Three rules are enforced by the shape rather than by documentation:
///
/// * **[`wants`](Self::wants) has no default.** Every sink has to write down
///   which transfers it cares about, in code, before it is ever handed one. A
///   default of `true` is how a hashing sink ends up hashing every byte of every
///   copy, which is exactly the thing this seam exists to prevent.
/// * **[`accept`](Self::accept) is synchronous.** It runs on the blocking pool
///   through [`BackendRuntime::blocking`], so a sink is free to read files, walk
///   directories, and talk to SQLite the obvious way — with no `async` colouring
///   and no chance of a long `std::fs` call landing on a runtime worker.
/// * **`&self`, not `&mut self`.** Sinks are shared and re-entered across jobs,
///   so state lives behind whatever lock the sink needs. The pipeline never
///   promises a sink exclusive access to itself; it only promises not to call it
///   from two places at once.
pub trait PostTransferSink: Send + Sync + 'static {
    /// A short, stable name, for the "which sink was slow" question.
    fn name(&self) -> &'static str;

    /// Whether this completion is worth the work. Called on the pipeline's
    /// thread, so it must be cheap: a field test, not a filesystem walk.
    fn wants(&self, completed: &TransferCompleted) -> bool;

    /// Do the work. Blocking, and free to take as long as it takes — nothing
    /// interactive is waiting on it.
    fn accept(&self, completed: &TransferCompleted);
}

/// The publish end of the seam.
///
/// Cheap to clone and cheap to hold. With no sinks it is a `None` and every
/// method is a branch.
#[derive(Clone, Debug)]
pub struct Pipeline {
    tx: Option<mpsc::Sender<TransferCompleted>>,
}

impl Pipeline {
    /// A pipeline with nothing behind it: no channel, no task, no cost.
    ///
    /// This is the honest state for a build with no sinks registered, and it is
    /// what the scheduler's tests use — a transfer test should not be exercising
    /// an indexer.
    pub fn inert() -> Self {
        Self { tx: None }
    }

    /// Register `sinks` and start the single drain task that feeds them.
    ///
    /// Returns [`inert`](Self::inert) when there is nothing to feed, or when the
    /// runtime is already shutting down and would refuse the task — either way
    /// the caller gets a pipeline that works, it just has no listeners.
    pub fn new(rt: &'static BackendRuntime, sinks: Vec<Arc<dyn PostTransferSink>>) -> Self {
        if sinks.is_empty() {
            return Self::inert();
        }

        let names: Vec<&'static str> = sinks.iter().map(|sink| sink.name()).collect();
        let (tx, rx) = mpsc::channel(QUEUE_DEPTH);
        let stop = rt.shutdown_token().child_token();

        if rt.spawn(drain(rt, sinks, rx, stop)).is_none() {
            tracing::debug!(
                target: "piku::transfer",
                "post-transfer sinks not started: shutting down"
            );
            return Self::inert();
        }

        tracing::info!(
            target: "piku::transfer",
            sinks = ?names,
            "post-transfer pipeline started"
        );
        Self { tx: Some(tx) }
    }

    /// Whether anything is listening.
    ///
    /// Checked *before* a payload is built, so a job with no sinks behind it does
    /// not clone a path list for nobody.
    pub fn has_sinks(&self) -> bool {
        self.tx.is_some()
    }

    /// Hand a completion over, or drop it.
    ///
    /// Never blocks and never fails visibly: the caller is a transfer that has
    /// already finished, and there is nothing useful it could do with an error.
    /// A drop is logged at `warn`, because a queue 64 completions deep that is
    /// still full means a sink is wedged, and that is worth seeing.
    pub fn publish(&self, completed: TransferCompleted) {
        let Some(tx) = self.tx.as_ref() else {
            return;
        };

        let id = completed.id;
        if tx.try_send(completed).is_err() {
            tracing::warn!(
                target: "piku::transfer",
                job = %id,
                depth = QUEUE_DEPTH,
                "post-transfer queue full or closed; completion dropped"
            );
        }
    }
}

/// The single drain task: one completion at a time, one sink at a time.
///
/// The serialization is the guarantee, not an implementation detail. Four copies
/// may be moving bytes while this runs, and this must not become a fifth
/// contender for the same disk.
async fn drain(
    rt: &'static BackendRuntime,
    sinks: Vec<Arc<dyn PostTransferSink>>,
    mut rx: mpsc::Receiver<TransferCompleted>,
    stop: CancellationToken,
) {
    loop {
        let completed = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            received = rx.recv() => match received {
                Some(completed) => completed,
                // Every `Pipeline` clone is gone, so no completion can ever
                // arrive again.
                None => return,
            },
        };

        let completed = Arc::new(completed);
        for sink in &sinks {
            // Checked between sinks rather than around the call below. A sink
            // that has started writing — an index segment, a row, a thumbnail
            // file — has to be allowed to finish that write; abandoning it is
            // how a half-built index outlives the process that built it.
            if stop.is_cancelled() {
                return;
            }
            if !sink.wants(&completed) {
                continue;
            }

            let name = sink.name();
            let span = tracing::info_span!(
                target: "piku::transfer",
                parent: None,
                "transfer.sink",
                sink = name,
                job = %completed.id,
            );

            let sink = Arc::clone(sink);
            let payload = Arc::clone(&completed);
            let started = Instant::now();

            // `blocking` captures `Span::current()` at the call, not at the
            // await, so the span has to be entered here for the sink's own log
            // lines to land under it.
            let work = span.in_scope(|| rt.blocking(move || sink.accept(&payload)));

            // Matched rather than ignored: a sink that panics takes down its own
            // completion and nothing else. The pipeline is infrastructure, and
            // infrastructure that dies with its first bad tenant is worse than
            // no infrastructure.
            match work.instrument(span).await {
                Ok(()) => tracing::debug!(
                    target: "piku::transfer",
                    sink = name,
                    ms = started.elapsed().as_millis(),
                    "sink finished"
                ),
                Err(error) => tracing::warn!(
                    target: "piku::transfer",
                    sink = name,
                    error = %error,
                    "sink did not finish"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use parking_lot::Mutex;

    use crate::backend::runtime;

    const DEADLINE: Duration = Duration::from_secs(10);

    /// A sink that counts what it is given, and can be told to want nothing or
    /// to block until released.
    struct Spy {
        label: &'static str,
        greedy: bool,
        seen: AtomicUsize,
        order: Mutex<Vec<u64>>,
        hold: AtomicBool,
    }

    impl Spy {
        fn new(label: &'static str, greedy: bool) -> Arc<Self> {
            Arc::new(Self {
                label,
                greedy,
                seen: AtomicUsize::new(0),
                order: Mutex::new(Vec::new()),
                hold: AtomicBool::new(false),
            })
        }

        fn seen(&self) -> usize {
            self.seen.load(Ordering::SeqCst)
        }
    }

    impl PostTransferSink for Spy {
        fn name(&self) -> &'static str {
            self.label
        }

        fn wants(&self, _completed: &TransferCompleted) -> bool {
            self.greedy
        }

        fn accept(&self, completed: &TransferCompleted) {
            self.order.lock().push(completed.id.0);
            self.seen.fetch_add(1, Ordering::SeqCst);
            while self.hold.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    fn completion() -> TransferCompleted {
        TransferCompleted {
            id: JobId::next(),
            kind: TransferKind::Copy,
            ending: Ending::Complete,
            sources: vec![PathBuf::from("/src/a")],
            destination: Some(PathBuf::from("/dst")),
            targets: vec![PathBuf::from("/dst/a")],
            summary: TransferSummary {
                items: 4,
                bytes: 40,
                ..TransferSummary::empty(TransferKind::Copy)
            },
        }
    }

    fn eventually(predicate: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + DEADLINE;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        false
    }

    /// The state this stage ships in has to be free, not merely harmless.
    #[test]
    fn an_inert_pipeline_has_nothing_behind_it() {
        let pipeline = Pipeline::inert();
        assert!(!pipeline.has_sinks());
        // Publishing into it is a null check, not a panic and not a leak.
        pipeline.publish(completion());

        // An empty sink list is the same thing, and never spawns a task.
        if let Some(rt) = runtime::get() {
            assert!(!Pipeline::new(rt, Vec::new()).has_sinks());
        }
    }

    /// `wants` is the gate that keeps a hashing sink from hashing every copy, so
    /// a `false` has to mean the sink is never entered at all.
    #[test]
    fn a_sink_that_wants_nothing_is_never_called() {
        let Some(rt) = runtime::get() else {
            return;
        };

        let greedy = Spy::new("greedy", true);
        let picky = Spy::new("picky", false);
        let pipeline = Pipeline::new(
            rt,
            vec![
                Arc::clone(&greedy) as Arc<dyn PostTransferSink>,
                Arc::clone(&picky) as Arc<dyn PostTransferSink>,
            ],
        );
        assert!(pipeline.has_sinks());

        let first = completion();
        let second = completion();
        let ids = [first.id.0, second.id.0];
        pipeline.publish(first);
        pipeline.publish(second);

        assert!(
            eventually(|| greedy.seen() == 2),
            "the greedy sink saw {} of 2",
            greedy.seen()
        );
        assert_eq!(picky.seen(), 0, "a sink ran against its own `wants`");
        assert_eq!(
            *greedy.order.lock(),
            ids.to_vec(),
            "completions arrived out of order"
        );
    }

    /// The property the whole module exists for: a wedged sink cannot reach back
    /// and slow the transfer that published to it.
    #[test]
    fn a_wedged_sink_drops_completions_instead_of_blocking_the_publisher() {
        let Some(rt) = runtime::get() else {
            return;
        };

        let slow = Spy::new("slow", true);
        slow.hold.store(true, Ordering::SeqCst);
        let pipeline = Pipeline::new(rt, vec![Arc::clone(&slow) as Arc<dyn PostTransferSink>]);

        // Wait until the sink is actually stuck, so the channel is the only
        // thing left to absorb the rest.
        pipeline.publish(completion());
        assert!(eventually(|| slow.seen() == 1), "the sink never started");

        // Far more than the queue can hold. Every one of these must return
        // immediately whether it fits or not.
        let overflow = QUEUE_DEPTH * 2;
        let started = Instant::now();
        for _ in 0..overflow {
            pipeline.publish(completion());
        }
        let spent = started.elapsed();
        assert!(
            spent < Duration::from_secs(1),
            "publishing {overflow} completions against a wedged sink took {spent:?}"
        );

        slow.hold.store(false, Ordering::SeqCst);

        // One in the sink plus one queue's worth is the ceiling, so the rest
        // were dropped — which is the intended outcome, not a failure.
        assert!(
            eventually(|| slow.seen() >= 2),
            "the queue never drained after the sink was released"
        );
        assert!(
            slow.seen() <= QUEUE_DEPTH + 1,
            "the queue held {} completions, past its {QUEUE_DEPTH} bound",
            slow.seen()
        );
    }

    #[test]
    fn what_a_completion_says_it_did_matches_its_kind() {
        let copy = completion();
        assert!(copy.wrote_anything());
        assert!(!copy.removed_anything(), "a copy destroyed its source");

        let mut moved = completion();
        moved.kind = TransferKind::Move;
        assert!(moved.wrote_anything() && moved.removed_anything());

        let mut deleted = completion();
        deleted.kind = TransferKind::DeletePermanent;
        deleted.destination = None;
        deleted.targets.clear();
        assert!(!deleted.wrote_anything(), "a delete wrote something");
        assert!(deleted.removed_anything());

        // Nothing happened, so there is nothing for a sink to look at.
        let mut empty = completion();
        empty.ending = Ending::Partial;
        empty.summary = TransferSummary::empty(TransferKind::Copy);
        assert!(!empty.wrote_anything() && !empty.removed_anything());
    }
}
