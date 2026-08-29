//! The scheduler: what runs, when, and who gets told about it.
//!
//! Three things live here, and they are deliberately not three modules — each
//! one is only correct in terms of the other two.
//!
//! * **Admission.** A permit pool bounds how many jobs move bytes at once, and
//!   priority decides which one gets a permit when one appears.
//! * **The driver.** One async task per job, sitting between the blocking
//!   closures that do the work. Pause and decision points live in the gaps.
//! * **The tick.** One 200 ms timer for the whole subsystem, and the only thing
//!   that publishes.
//!
//! # Why the tick is the only publisher
//!
//! The alternative is for the copy loop to send an update per chunk. At 1 MB
//! chunks and 500 MB/s that is five hundred sends a second, each one waking the
//! render thread, for a bar that can only move one pixel every few thousand of
//! them. So the copy loop writes to atomics in
//! [`TransferControl`](super::control::TransferControl) — one relaxed
//! `fetch_add` per chunk — and the tick reads them five times a second and
//! publishes once for every live job together.
//!
//! Everything else follows from that. State changes are detected by comparing
//! [`TransferControl::revision`](super::control::TransferControl::revision)
//! against what was last sent, which is how "send only on change" works without
//! `PartialEq` on a state containing an `io::Error`. Failure rows come out of
//! [`take_new_failures`](super::control::TransferControl::take_new_failures).
//! `Pausing → Paused` for a worker parked mid-chunk is settled here too, because
//! that worker is inside a condvar and cannot publish for itself.
//!
//! # Why the batch is all-or-nothing
//!
//! The tick uses [`StreamSink::try_send`], so a UI that has stopped draining
//! cannot throttle the disk. That means a batch can be dropped, so nothing is
//! committed until one lands: published revisions, unsent failure rows, and
//! removal of finished jobs all happen after the send, not before. A dropped
//! batch is re-derived 200 ms later, identically. The throughput ring is the one
//! exception, and it has to be: it is a function of elapsed time, not of
//! delivery.
//!
//! # Why the permit is dropped rather than held while stopped
//!
//! A job waiting on a modal is not using the disk. If it kept its permit,
//! "Pause All" on four jobs would leave four permits parked and a queued job
//! waiting on nobody. So [`Driver::release`] hands the permit back before every
//! await that can last, and takes a fresh one afterwards. Re-acquisition goes
//! straight to the semaphore rather than back through the priority queue,
//! because a paused job is
//! [`StateGroup::Active`](super::job::StateGroup::Active) — it never left.
//!
//! One case deliberately does *not* release: a pause that lands part-way through
//! a file. `copy_file` parks between chunks with both handles open and the read
//! offset intact, which is what makes resume free, and there is no way to hand
//! that to another job. So the permit is released at a *file* boundary, and a
//! job paused inside a 4 GB file keeps it until that file is resumed and
//! finished. `tests::a_pause_inside_a_file_keeps_its_permit` pins it, because it
//! is the kind of thing a later refactor would "fix" by abandoning the write.
//!
//! # A deviation from the plan: no `JoinSet`
//!
//! The plan called for per-job `JoinSet` fan-out with `abort_all` on cancel.
//! Three reasons it is not here:
//!
//! * There is no fan-out to track. A job runs exactly one blocking closure at a
//!   time — [`CopyPass::run`] walks its own worklist — so a set of one is a
//!   `JoinHandle` with extra steps.
//! * `abort_all` cannot interrupt a `spawn_blocking` closure. Tokio aborts a
//!   blocking task only *before* it starts; once running it is a real thread
//!   running real `write` calls, and nothing outside it can stop it. The only
//!   working cancellation is the flag the loop polls, which is what
//!   [`TransferControl::cancel`](super::control::TransferControl::cancel) sets.
//! * Draining already exists.
//!   [`BackendRuntime::spawn`](crate::backend::runtime::BackendRuntime::spawn)
//!   registers every task with a `TaskTracker` that `shutdown()` waits on, so a
//!   second tracker here would only be a second thing to forget to update.
//!
//! For the same reason the blocking hops are awaited rather than raced against
//! the shutdown token, unlike
//! [`drive`](crate::backend::services::drive). Abandoning an enumeration is
//! free; abandoning a `copy_file` mid-write is how partial files are left
//! behind.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{MissedTickBehavior, interval};
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::backend::error::TransferError;
use crate::backend::path::PathPolicy;
use crate::backend::protocol::{BackendStream, Progress, StreamItem, StreamSink, stream_channel};
use crate::backend::runtime::BackendRuntime;
use crate::backend::services::transfer::conflict::{Conflict, ConflictResolver, Decision, Outcome};
use crate::backend::services::transfer::control::TransferControl;
use crate::backend::services::transfer::copy::{CopyContext, CopyPass, Pass};
use crate::backend::services::transfer::enumerate::{Plan, enumerate};
use crate::backend::services::transfer::job::{
    JobId, Priority, TransferFailure, TransferKind, TransferRequest, TransferState,
    TransferSummary, TransferUpdate,
};
use crate::backend::services::transfer::pipeline::{Ending, Pipeline, TransferCompleted};
use crate::backend::services::transfer::precheck::{self, Approved};
use crate::backend::services::transfer::rate::{RateRing, TICK};
use crate::security::audit;
use crate::storage::provider::StorageProvider;

/// Jobs allowed to move bytes at once.
///
/// Four, and not more, because concurrency on one spindle is not throughput:
/// two copies on the same disk interleave seeks and finish later than the same
/// two run in sequence. Four is enough to keep several *different* volumes busy,
/// which is the case that does benefit.
///
/// Fixed at construction. Changing it while jobs run needs permit-debt
/// bookkeeping — `Semaphore::forget_permits` only takes what is idle right now,
/// so lowering the limit under load silently does nothing — and the clean fix is
/// a ballast task holding `n` permits, which is a change to make when there is a
/// setting to drive it.
pub const CONCURRENCY: usize = 4;

/// Batches the event channel holds before the tick starts dropping them.
///
/// 64 at 5 Hz is roughly thirteen seconds of slack. A consumer further behind
/// than that is not going to catch up by being waited for.
const EVENT_BATCHES: usize = 64;

/// Audit ops written from here. Job-level, one line per job, matching
/// `services::jobs::queue`'s one-record-per-fetch shape rather than
/// `storage::local`'s one-record-per-file.
const AUDIT_PAUSE: &str = "transfer.pause";
const AUDIT_RESUME: &str = "transfer.resume";
const AUDIT_CANCEL: &str = "transfer.cancel";

/// One job's periodic reading.
///
/// Completion and activity are separate signals and stay separate all the way
/// out: [`progress`](Self::progress) drives the completion ring and
/// [`waveform`](Self::waveform) drives the throughput bars. A transfer stuck on
/// a 4 GB file at 92% has a healthy-looking ring and a floor-flat waveform, and
/// the waveform is the true reading.
#[derive(Clone, Debug)]
pub struct Sample {
    pub id: JobId,
    /// Bytes and items done against planned. Zero totals until the scan
    /// finishes, which reads as indeterminate.
    pub progress: Progress,
    /// Normalized `0.0..=1.0`, oldest first, one per ring slot.
    pub waveform: Vec<f32>,
    /// Absolute, for the numeric row. Normalization is for the shape only.
    pub bytes_per_sec: u64,
    /// `None` until there are enough samples to mean anything.
    pub eta: Option<Duration>,
    /// Sanitized file name of the item in flight — never a full path.
    pub current_item: Option<Arc<str>>,
}

/// What the subsystem publishes.
///
/// One stream for every job, not one per job: the consumer is a single store
/// that owns the whole list, and a stream per job would mean a subscription to
/// tear down on every completion.
#[derive(Clone, Debug)]
pub enum TransferEvent {
    /// Something happened to one job.
    Job { id: JobId, update: TransferUpdate },
    /// One reading per live job, all of them, once per period.
    Tick(Vec<Sample>),
    /// Terminal counts. Arrives with the terminal `Job` event, and is the last
    /// thing said about that id.
    Finished { id: JobId, summary: TransferSummary },
}

/// A submitted job that has not been given a permit yet.
struct Pending {
    control: Arc<TransferControl>,
    request: TransferRequest,
}

/// The two paths an audit record names.
///
/// Kept because a pause or a cancel can be recorded long after the driver
/// consumed the request that carried them.
#[derive(Clone, Debug)]
struct Trail {
    source: PathBuf,
    destination: Option<PathBuf>,
}

/// Registry entry: the shared cell, plus the sampling state that belongs to the
/// tick rather than to the job.
struct Tracked {
    control: Arc<TransferControl>,
    trail: Trail,
    ring: RateRing,
    /// `done_bytes` at the previous tick, so the ring gets a delta.
    last_bytes: u64,
    /// The revision last successfully published. `None` means nothing has been
    /// sent for this job yet, so its first state always goes out.
    published: Option<u64>,
    /// Failure rows taken from the control but not yet delivered. Bounded by
    /// the control's own retention cap.
    unsent: Vec<TransferFailure>,
}

impl Tracked {
    fn new(control: Arc<TransferControl>, trail: Trail) -> Self {
        Self {
            control,
            trail,
            ring: RateRing::new(),
            last_bytes: 0,
            published: None,
            unsent: Vec::new(),
        }
    }
}

struct Inner {
    rt: &'static BackendRuntime,
    policy: PathPolicy,
    provider: Arc<dyn StorageProvider>,
    /// Where a finished job is announced. [`Pipeline::inert`] until a sink
    /// exists, and checked before a payload is built, so the whole seam costs
    /// one branch per job today.
    pipeline: Pipeline,
    /// One deque, drained by priority rather than three deques drained in
    /// order. The observable behavior is the same — see [`Inner::take_next`] —
    /// and re-prioritizing a queued job becomes a field write instead of queue
    /// surgery.
    queue: Mutex<VecDeque<Pending>>,
    permits: Arc<Semaphore>,
    /// `BTreeMap` because `JobId` is ordered and monotonic, so iteration is
    /// submission order. A `HashMap` would reshuffle the transfer center's list
    /// on every tick.
    jobs: Mutex<BTreeMap<JobId, Tracked>>,
    sink: Mutex<Option<StreamSink<TransferEvent>>>,
    /// Signals the admission loop that the queue went from empty to not.
    wake: tokio::sync::Notify,
    /// Fires on [`Scheduler::shutdown`] *or* runtime shutdown, since it is a
    /// child of the runtime's own token.
    stop: CancellationToken,
}

/// The engine's scheduling half. Cheap to clone; there is one.
#[derive(Clone)]
pub struct Scheduler(Arc<Inner>);

impl Scheduler {
    pub fn new(
        rt: &'static BackendRuntime,
        policy: PathPolicy,
        provider: Arc<dyn StorageProvider>,
        pipeline: Pipeline,
    ) -> Self {
        Self::with_concurrency(rt, policy, provider, pipeline, CONCURRENCY)
    }

    /// Construct with an explicit permit count. For tests that need "exactly one
    /// runs" to be a fact rather than a race.
    pub fn with_concurrency(
        rt: &'static BackendRuntime,
        policy: PathPolicy,
        provider: Arc<dyn StorageProvider>,
        pipeline: Pipeline,
        concurrency: usize,
    ) -> Self {
        let inner = Arc::new(Inner {
            rt,
            policy,
            provider,
            pipeline,
            queue: Mutex::new(VecDeque::new()),
            permits: Arc::new(Semaphore::new(concurrency.max(1))),
            jobs: Mutex::new(BTreeMap::new()),
            sink: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
            stop: rt.shutdown_token().child_token(),
        });

        if rt.spawn(admit(Arc::clone(&inner))).is_none() {
            tracing::warn!(target: "piku::transfer", "scheduler started during shutdown");
        }
        if rt.spawn(sample(Arc::clone(&inner))).is_none() {
            tracing::warn!(target: "piku::transfer", "sample tick not started");
        }

        Self(inner)
    }

    /// Take the event stream, replacing any previous one.
    ///
    /// One consumer, by design: the gpui-side store owns the whole job list, so
    /// a second subscriber would be a second copy of it to keep in step. Calling
    /// this again is how a rebuilt store re-attaches; the old sink is dropped
    /// and its stream ends.
    pub fn subscribe(&self) -> BackendStream<TransferEvent> {
        let (sink, stream) = stream_channel(EVENT_BATCHES);
        *self.0.sink.lock() = Some(sink);
        stream
    }

    /// Queue a job. Returns immediately with the id the UI can act on.
    pub fn submit(&self, request: TransferRequest) -> JobId {
        let id = JobId::next();
        let control = TransferControl::new(id, request.kind, request.priority);
        let trail = Trail {
            source: request.sources.first().cloned().unwrap_or_default(),
            destination: request.destination.clone(),
        };

        self.0
            .jobs
            .lock()
            .insert(id, Tracked::new(Arc::clone(&control), trail));
        self.0.queue.lock().push_back(Pending {
            control: Arc::clone(&control),
            request,
        });
        self.0.wake.notify_one();

        tracing::info!(
            target: "piku::transfer",
            job = %id,
            kind = ?control.kind(),
            priority = control.priority().label(),
            "queued"
        );
        id
    }

    /// Ask a job to stop at its next safe point.
    ///
    /// Works on a queued job too: the request stands, and the driver honours it
    /// the moment it is dispatched. That is what makes "Pause All" cover the
    /// whole list rather than only the running part of it.
    pub fn pause(&self, id: JobId) -> bool {
        let Some(control) = self.control(id) else {
            return false;
        };
        let state = control.state();
        // A terminal job has nothing to pause. A job waiting for a decision is
        // already stopped, and pausing it would leave two reasons to be stopped
        // and one way to lift them.
        if state.is_terminal()
            || matches!(state, TransferState::WaitingForInput { .. })
            || control.is_pause_requested()
        {
            return false;
        }
        control.request_pause();
        self.audit(AUDIT_PAUSE, id, true, state.label());
        true
    }

    /// Lift a pause, whatever state the job reached while it was standing.
    ///
    /// Keyed off the pause request rather than the state, because a job paused
    /// while still `Queued` never displays `Paused` and would otherwise be
    /// un-resumable.
    pub fn resume(&self, id: JobId) -> bool {
        let Some(control) = self.control(id) else {
            return false;
        };
        if control.state().is_terminal() || !control.is_pause_requested() {
            return false;
        }
        control.request_resume();
        self.audit(AUDIT_RESUME, id, true, "");
        true
    }

    pub fn pause_all(&self) -> usize {
        self.ids().into_iter().filter(|id| self.pause(*id)).count()
    }

    pub fn resume_all(&self) -> usize {
        self.ids().into_iter().filter(|id| self.resume(*id)).count()
    }

    /// Stop a job for good.
    pub fn cancel(&self, id: JobId) -> bool {
        let Some(control) = self.control(id) else {
            return false;
        };
        if control.state().is_terminal() {
            return false;
        }

        control.cancel();
        self.audit(AUDIT_CANCEL, id, true, control.state().label());

        // A job that never started has no chunk to drain and no driver to claim
        // it, so nothing else will ever mark it terminal. A running one is
        // claimed by its driver, at the boundary where it actually stops.
        if matches!(control.state(), TransferState::Queued) {
            self.0
                .queue
                .lock()
                .retain(|pending| pending.control.id() != id);
            control.set_state(TransferState::Cancelled);
        }
        true
    }

    /// Answer a job that is waiting for input.
    pub fn resolve(&self, id: JobId, decision: Decision) -> bool {
        self.control(id)
            .is_some_and(|control| control.deliver(decision))
    }

    /// Change a job's priority. Only meaningful while it is queued, which is
    /// where the kebab menu offers it.
    pub fn prioritize(&self, id: JobId, priority: Priority) -> bool {
        let Some(control) = self.control(id) else {
            return false;
        };
        control.set_priority(priority);
        true
    }

    /// Live jobs in submission order, for a consumer that subscribed after they
    /// started. Terminal jobs are gone from here the moment their events land —
    /// the store is the history, not this.
    pub fn states(&self) -> Vec<(JobId, TransferState)> {
        self.0
            .jobs
            .lock()
            .iter()
            .map(|(id, tracked)| (*id, tracked.control.state()))
            .collect()
    }

    /// Cancel everything and stop the loops.
    ///
    /// Idempotent, and reached by two paths: explicitly from
    /// `Backend::shutdown`, and implicitly through the runtime's token, since
    /// [`Inner::stop`] is its child. Blocking work already in flight is not
    /// interrupted here — it polls the cancel flag and unwinds itself, which is
    /// what leaves no partial files.
    pub fn shutdown(&self) {
        for id in self.ids() {
            self.cancel(id);
        }
        self.0.stop.cancel();
        self.0.permits.close();
        self.0.wake.notify_waiters();
    }

    fn control(&self, id: JobId) -> Option<Arc<TransferControl>> {
        self.0
            .jobs
            .lock()
            .get(&id)
            .map(|tracked| Arc::clone(&tracked.control))
    }

    fn ids(&self) -> Vec<JobId> {
        self.0.jobs.lock().keys().copied().collect()
    }

    /// One audit line, with the guard released before the write.
    fn audit(&self, op: &str, id: JobId, ok: bool, detail: &str) {
        let trail = self.0.jobs.lock().get(&id).map(|t| t.trail.clone());
        if let Some(trail) = trail {
            audit::record(op, &trail.source, trail.destination.as_deref(), ok, detail);
        }
    }
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("queued", &self.0.queue.lock().len())
            .field("tracked", &self.0.jobs.lock().len())
            .field("free_permits", &self.0.permits.available_permits())
            .finish()
    }
}

impl Inner {
    /// The highest-priority job that has waited longest.
    ///
    /// `min_by_key` returns the first of several equal minima, which is exactly
    /// the FIFO guarantee inside a priority band. So one deque plus this is
    /// observably three FIFO queues drained `Interactive → Normal →
    /// Background`, and `Priority`'s discriminant order is what encodes that.
    fn take_next(&self) -> Option<Pending> {
        let mut queue = self.queue.lock();
        let index = queue
            .iter()
            .enumerate()
            .min_by_key(|(_, pending)| pending.control.priority())
            .map(|(index, _)| index)?;
        queue.remove(index)
    }

    /// Sample every live job and publish whatever changed.
    fn publish(&self, elapsed: Duration) {
        let mut events = Vec::new();
        let mut samples = Vec::new();
        // Applied only if the batch lands.
        let mut sent_revisions: Vec<(JobId, u64)> = Vec::new();
        let mut forget: Vec<JobId> = Vec::new();

        {
            let mut jobs = self.jobs.lock();
            for (id, tracked) in jobs.iter_mut() {
                let control = &tracked.control;
                // A worker parked mid-chunk is inside a condvar and cannot
                // report that it stopped. This is where `Pausing` becomes
                // `Paused` for that case; the driver publishes its own.
                control.settle_pause_state();

                let state = control.state();
                let progress = control.progress();

                let delta = progress.done_bytes.saturating_sub(tracked.last_bytes);
                tracked.last_bytes = progress.done_bytes;
                // A deliberately stopped job is idle, not stalled: a run of
                // zeros in the ETA window would leave it reporting no estimate
                // long after it resumed.
                if matches!(state, TransferState::Running | TransferState::Pausing) {
                    tracked.ring.push(delta, elapsed);
                } else {
                    tracked.ring.idle();
                }

                samples.push(Sample {
                    id: *id,
                    progress,
                    waveform: tracked.ring.samples(),
                    bytes_per_sec: tracked.ring.bytes_per_sec(),
                    eta: tracked.ring.eta(control.remaining_bytes()),
                    current_item: control.current(),
                });

                let revision = control.revision();
                if tracked.published != Some(revision) {
                    events.push(TransferEvent::Job {
                        id: *id,
                        update: TransferUpdate::State(state.clone()),
                    });
                    sent_revisions.push((*id, revision));
                }

                // Buffered in the tracker rather than sent straight out, so a
                // dropped batch does not lose rows the summary would then
                // contradict.
                tracked.unsent.extend(control.take_new_failures());
                for failure in &tracked.unsent {
                    events.push(TransferEvent::Job {
                        id: *id,
                        update: TransferUpdate::Failed(failure.clone()),
                    });
                }

                if state.is_terminal() {
                    events.push(TransferEvent::Finished {
                        id: *id,
                        summary: summarize(control),
                    });
                    forget.push(*id);
                }
            }
        }

        if samples.is_empty() {
            return;
        }
        events.push(TransferEvent::Tick(samples));

        if !self.send(events) {
            return;
        }

        let mut jobs = self.jobs.lock();
        for (id, revision) in sent_revisions {
            if let Some(tracked) = jobs.get_mut(&id) {
                tracked.published = Some(revision);
                tracked.unsent.clear();
            }
        }
        for id in forget {
            jobs.remove(&id);
        }
    }

    /// Whether the batch may be treated as delivered.
    ///
    /// `true` with no subscriber at all, which is not a lie so much as the only
    /// workable answer: holding batches for a consumer that does not exist would
    /// keep every finished job's failure list alive for the life of the process.
    fn send(&self, events: Vec<TransferEvent>) -> bool {
        let mut sink = self.sink.lock();
        match sink.as_ref() {
            None => true,
            Some(active) if active.is_cancelled() => {
                // The consumer went away. Drop the sink so later ticks are free.
                *sink = None;
                true
            }
            Some(active) => active.try_send(StreamItem::Batch(events)),
        }
    }
}

fn summarize(control: &TransferControl) -> TransferSummary {
    let progress = control.progress();
    TransferSummary {
        kind: control.kind(),
        items: progress.done_items,
        bytes: progress.done_bytes,
        skipped: control.skipped(),
        replaced: control.replaced(),
        failures: control.failures(),
    }
}

/// The admission loop: wait for work, take a permit, *then* choose a job.
///
/// The order matters. Choosing first and then waiting for a permit would fix the
/// decision at submission time, so a `Background` job submitted while the pool
/// was full would hold the next permit against an `Interactive` job submitted a
/// second later. Choosing after the permit appears re-evaluates priority at the
/// only moment it means anything.
async fn admit(inner: Arc<Inner>) {
    loop {
        if inner.stop.is_cancelled() {
            return;
        }

        let empty = inner.queue.lock().is_empty();
        if empty {
            // `notify_one` stores a permit when nobody is waiting, so a submit
            // landing between the check and the await cannot be missed.
            inner.wake.notified().await;
            continue;
        }

        // Deliberately not inside a `select!`. Tokio's `Semaphore` is fair and
        // FIFO, and a cancelled `acquire` loses its place in that queue — which
        // for the admission loop means losing the turn it just waited for.
        // Shutdown closes the semaphore instead, which fails the acquire.
        let Ok(permit) = Arc::clone(&inner.permits).acquire_owned().await else {
            return;
        };

        let Some(pending) = inner.take_next() else {
            // Cancelled out from under us between the check and the pop. The
            // permit drops here, which is the whole recovery.
            continue;
        };

        let control = Arc::clone(&pending.control);
        if inner
            .rt
            .spawn(drive(Arc::clone(&inner), pending, permit))
            .is_none()
        {
            // Shutting down: nothing will ever claim it, so claim it here.
            control.cancel();
            control.set_state(TransferState::Cancelled);
            return;
        }
    }
}

/// The sample tick. One timer, every job, five times a second.
async fn sample(inner: Arc<Inner>) {
    let mut timer = interval(TICK);
    // A loaded machine delivers the timer late. Firing the missed ticks
    // back-to-back would publish several batches with near-zero elapsed time
    // between them, which reads as a throughput spike that never happened.
    timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut last = Instant::now();
    loop {
        tokio::select! {
            biased;
            () = inner.stop.cancelled() => return,
            _ = timer.tick() => {}
        }

        let now = Instant::now();
        let elapsed = now.duration_since(last);
        last = now;
        inner.publish(elapsed);
    }
}

/// Why a job stopped, for the paths that are not failures.
enum Stop {
    Completed,
    Cancelled,
}

/// The byte-moving pass and the two things it borrows, bundled so they can cross
/// a thread boundary together.
///
/// [`BackendRuntime::blocking`] needs `FnOnce() + Send + 'static`, so the pass
/// cannot be borrowed into the closure — it has to be moved in and handed back.
struct Bundle {
    pass: CopyPass,
    plan: Plan,
    resolver: ConflictResolver,
}

/// The approved shape of a job, kept aside for the post-transfer publish.
///
/// A field on the driver rather than a return value, because the interesting
/// case is the one that *failed* part-way: a copy that wrote 300 of 400 files
/// and then hit a read error has changed the filesystem, and a sink that only
/// hears about clean completions is a sink with a stale index.
///
/// Only populated when a sink exists — see [`Pipeline::has_sinks`].
struct Shape {
    sources: Vec<PathBuf>,
    destination: Option<PathBuf>,
    targets: Vec<PathBuf>,
}

/// One job's async half.
struct Driver {
    inner: Arc<Inner>,
    control: Arc<TransferControl>,
    /// `None` while the job is deliberately stopped. See the module header on
    /// why this is dropped rather than held.
    permit: Option<OwnedSemaphorePermit>,
    /// `None` until the scan has run, and always `None` with no sinks behind the
    /// pipeline.
    shape: Option<Shape>,
}

async fn drive(inner: Arc<Inner>, pending: Pending, permit: OwnedSemaphorePermit) {
    let Pending { control, request } = pending;

    if control.is_cancelled() {
        control.set_state(TransferState::Cancelled);
        return;
    }

    let span = tracing::info_span!(
        target: "piku::transfer",
        parent: None,
        "transfer.job",
        job = %control.id(),
        kind = ?control.kind(),
        priority = control.priority().label(),
    );

    // Enough to audit with once the request itself has been consumed.
    let source = request.sources.first().cloned().unwrap_or_default();
    let destination = request.destination.clone();
    let kind = request.kind;

    let mut driver = Driver {
        inner,
        control: Arc::clone(&control),
        permit: Some(permit),
        shape: None,
    };
    let outcome = driver.run(request).instrument(span).await;

    // Free the permit before publishing the terminal state, so a queued job is
    // already moving by the time the UI redraws.
    driver.release();
    control.set_current(None);

    let (state, ok, detail) = match outcome {
        Ok(Stop::Completed) => {
            let progress = control.progress();
            (
                TransferState::Completed,
                true,
                format!(
                    "{} items, {} bytes, {} skipped, {} replaced, {} failed",
                    progress.done_items,
                    progress.done_bytes,
                    control.skipped(),
                    control.replaced(),
                    control.failure_count()
                ),
            )
        }
        Ok(Stop::Cancelled) => (TransferState::Cancelled, false, "cancelled".to_owned()),
        Err(error) => {
            let detail = error.to_string();
            (TransferState::Failed(Arc::new(error)), false, detail)
        }
    };

    // A clean run is one where every item the plan named actually landed. A skip
    // and a per-item failure both mean the tree on disk is a subset of the plan,
    // which is the distinction a sink needs before it trusts a path.
    let ending = if matches!(state, TransferState::Completed)
        && control.failure_count() == 0
        && control.skipped() == 0
    {
        Ending::Complete
    } else {
        Ending::Partial
    };

    tracing::info!(
        target: "piku::transfer",
        job = %control.id(),
        state = state.label(),
        detail = %detail,
        "finished"
    );

    // One record per job, not per file. `DeletePermanent` is excluded because it
    // is already audited twice over — `copy::destroy` records
    // `transfer.delete_permanent` per top-level source, and `storage::local`
    // records every individual removal.
    if !matches!(kind, TransferKind::DeletePermanent) {
        audit::record(
            kind.audit_op(),
            &source,
            destination.as_deref(),
            ok,
            &detail,
        );
    }

    control.set_state(state);

    // Last, and off the interactive path entirely: a queued job already took the
    // permit above, and the sinks run one at a time on a single blocking thread.
    // Nothing here can slow the copy that just finished or the one that started.
    if let Some(shape) = driver.shape.take() {
        driver.inner.pipeline.publish(TransferCompleted {
            id: control.id(),
            kind,
            ending,
            sources: shape.sources,
            destination: shape.destination,
            targets: shape.targets,
            summary: summarize(&control),
        });
    }
}

impl Driver {
    async fn run(&mut self, request: TransferRequest) -> Result<Stop, TransferError> {
        let TransferRequest {
            kind,
            sources,
            destination,
            policy,
            ..
        } = request;

        self.control.set_state(TransferState::Running);

        // Authorize the shape before anything else: resolved paths, no
        // self-descendant, no no-op move, and a real write probe at the
        // destination. Everything downstream assumes this passed.
        let approved = self.approve(kind, sources, destination).await?;

        let plan = self.scan(&approved).await?;
        self.control.set_totals(plan.total_bytes, plan.total_items);

        // Recorded here, before a byte moves, so a job that fails half way
        // through still tells the sinks where to look. Built only when something
        // is listening: with no sinks this is a branch, not three clones.
        if self.inner.pipeline.has_sinks() {
            self.shape = Some(Shape {
                sources: approved.sources.clone(),
                destination: approved.destination.clone(),
                targets: plan.items.iter().map(|item| item.target.clone()).collect(),
            });
        }

        let mut resolver = ConflictResolver::new(policy);

        // Everything answerable before a byte moves, asked once: the space
        // shortfall and every collision the scan found.
        let mut blocks = Vec::new();
        if let Some(destination) = approved.destination.clone()
            && let Some(block) = self.space(destination, plan.total_bytes).await
        {
            blocks.push(block);
        }
        blocks.extend(plan.conflicts.iter().cloned());
        self.settle(&mut resolver, blocks).await?;

        let mut bundle = Bundle {
            pass: CopyPass::new(kind, &approved.sources, &plan),
            plan,
            resolver,
        };
        let destination = approved.destination;

        loop {
            if self.control.is_cancelled() {
                return Ok(Stop::Cancelled);
            }

            let (returned, outcome) = self.one_pass(destination.clone(), bundle).await?;
            bundle = returned;

            match outcome {
                Pass::Done => return Ok(Stop::Completed),
                Pass::Cancelled => return Ok(Stop::Cancelled),
                Pass::Paused => self.park().await?,
                // Collisions the scan could not have seen: a name created
                // between the scan and the copy. Same resolver, same surface.
                Pass::NeedsDecision(pending) => {
                    self.settle(&mut bundle.resolver, pending).await?;
                }
            }
        }
    }

    async fn approve(
        &self,
        kind: TransferKind,
        sources: Vec<PathBuf>,
        destination: Option<PathBuf>,
    ) -> Result<Approved, TransferError> {
        let policy = self.inner.policy.clone();
        self.inner
            .rt
            .blocking(move || precheck::approve(&policy, kind, &sources, destination.as_deref()))
            .await
            .map_err(|_| TransferError::Cancelled)?
    }

    async fn scan(&self, approved: &Approved) -> Result<Plan, TransferError> {
        let policy = self.inner.policy.clone();
        let sources = approved.sources.clone();
        let destination = approved.destination.clone();
        let cancel = self.control.cancel_flag();
        self.inner
            .rt
            .blocking(move || enumerate(&policy, &sources, destination.as_deref(), &cancel))
            .await
            .map_err(|_| TransferError::Cancelled)?
    }

    /// The free-space check, which reports `None` both when there is room and
    /// when the volume could not be identified — a probe that cannot answer must
    /// not block the transfer.
    async fn space(&self, destination: PathBuf, planned: u64) -> Option<Conflict> {
        self.inner
            .rt
            .blocking(move || precheck::check_space(&destination, planned))
            .await
            .ok()
            .flatten()
    }

    /// One trip through the byte-moving pass.
    async fn one_pass(
        &self,
        destination: Option<PathBuf>,
        bundle: Bundle,
    ) -> Result<(Bundle, Pass), TransferError> {
        let provider = Arc::clone(&self.inner.provider);
        let policy = self.inner.policy.clone();
        let control = Arc::clone(&self.control);

        let joined = self.inner.rt.blocking(move || {
            // Destructured inside, so the pass can be borrowed mutably beside
            // the plan and the resolver it reads.
            let Bundle {
                mut pass,
                plan,
                resolver,
            } = bundle;
            let outcome = {
                let ctx = CopyContext {
                    provider: provider.as_ref(),
                    policy: &policy,
                    control: &control,
                    plan: &plan,
                    resolver: &resolver,
                    destination: destination.as_deref(),
                };
                pass.run(&ctx)
            };
            (
                Bundle {
                    pass,
                    plan,
                    resolver,
                },
                outcome,
            )
        });

        match joined.await {
            Ok((bundle, outcome)) => Ok((bundle, outcome?)),
            // The blocking worker is gone, which only happens on teardown.
            Err(_) => Err(TransferError::Cancelled),
        }
    }

    /// Hold the job in `WaitingForInput` until every block has an instruction,
    /// or the user gives up.
    ///
    /// Loops rather than asking once, because a `Decision` can be partial —
    /// `Each` covers the rows it was given and `All(Rename(_))` covers nothing.
    /// It cannot spin: every round needs a fresh answer, and a channel nobody
    /// answers becomes `Abandoned`.
    async fn settle(
        &mut self,
        resolver: &mut ConflictResolver,
        pending: Vec<Conflict>,
    ) -> Result<(), TransferError> {
        let mut asked = false;
        loop {
            if self.control.is_cancelled() {
                return Ok(());
            }

            let unresolved = resolver.unresolved(&pending);
            if unresolved.is_empty() {
                if asked {
                    self.reacquire().await?;
                    self.control.set_state(TransferState::Running);
                }
                return Ok(());
            }

            asked = true;
            self.release();
            let decision = self.ask(&unresolved).await?;
            if resolver.record(&decision, &unresolved) == Outcome::Cancel {
                // Recorded as a cancel rather than a failure: the user chose it.
                self.control.cancel();
                return Ok(());
            }
        }
    }

    /// Publish one question and wait for its answer.
    async fn ask(&self, pending: &[Conflict]) -> Result<Decision, TransferError> {
        // Armed *before* the state is published. The other order has a window
        // where a UI that answers instantly delivers into a channel that does
        // not exist yet, and the answer is lost.
        let answer = self.control.arm_decision();
        self.control.set_state(TransferState::WaitingForInput {
            pending: pending.to_vec(),
        });

        match answer.await {
            Ok(decision) => Ok(decision),
            Err(_) if self.control.is_cancelled() => Err(TransferError::Cancelled),
            // The sender was dropped rather than used: the window went away
            // while the question was open, or something re-armed it.
            Err(_) => Err(TransferError::Abandoned),
        }
    }

    /// Wait out a pause.
    async fn park(&mut self) -> Result<(), TransferError> {
        self.release();
        // The *task* is the thing that stopped, so the task says so. (The other
        // pause — parked mid-chunk inside `copy_file` — cannot, which is what
        // the tick's `settle_pause_state` call is for.)
        if !self.control.is_cancelled() {
            self.control.set_state(TransferState::Paused);
        }

        self.control.wait_for_resume().await;
        if self.control.is_cancelled() {
            return Ok(());
        }

        self.reacquire().await?;
        // Written after re-acquiring, and unconditionally: a resume that landed
        // between `release` and the line above already set `Running`, and the
        // `Paused` write would otherwise have stamped over it.
        self.control.set_state(TransferState::Running);
        Ok(())
    }

    /// Give the permit back. Idempotent.
    fn release(&mut self) {
        self.permit = None;
    }

    /// Take a permit, if this driver does not already hold one.
    ///
    /// Straight from the semaphore rather than back through the priority queue:
    /// a paused job is `StateGroup::Active` and never left, so re-queueing it
    /// would let a `Normal` job overtake a paused `Interactive` one every time
    /// it breathed.
    async fn reacquire(&mut self) -> Result<(), TransferError> {
        if self.permit.is_some() {
            return Ok(());
        }
        match Arc::clone(&self.inner.permits).acquire_owned().await {
            Ok(permit) => {
                self.permit = Some(permit);
                Ok(())
            }
            // Closed, which only happens on shutdown.
            Err(_) => Err(TransferError::Cancelled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::backend::runtime;
    use crate::backend::services::transfer::conflict::ConflictPolicy;
    use crate::core::entry::FsEntry;
    use crate::storage::local::LocalProvider;
    use crate::storage::provider::{PauseGate, ProgressFn};

    /// Generous, because CI boxes are slow and the alternative is a `sleep` and
    /// a guess.
    const DEADLINE: Duration = Duration::from_secs(10);

    /// A provider that holds every `copy_file` at the door until it is opened,
    /// and records the order in which jobs arrived.
    ///
    /// This is what makes "exactly one runs" a fact rather than a race: the
    /// first job cannot finish and free its permit while the gate is shut.
    struct Gate {
        open: AtomicBool,
        entered: AtomicUsize,
        seen: Mutex<Vec<PathBuf>>,
        /// Whether the copy forwards the job's [`PauseGate`], which decides
        /// *where* a pause can land. With it, a standing pause parks inside the
        /// file and the permit stays put. Without it — which the trait
        /// explicitly allows for a provider with nothing to suspend — the only
        /// suspension point is the pass's own, at a file boundary, which is the
        /// case that hands the permit over.
        suspendable: AtomicBool,
    }

    impl Gate {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                open: AtomicBool::new(false),
                entered: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                suspendable: AtomicBool::new(true),
            })
        }

        fn open(&self) {
            self.open.store(true, Ordering::SeqCst);
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }

        fn seen(&self) -> Vec<PathBuf> {
            self.seen.lock().clone()
        }
    }

    struct GatedProvider {
        local: LocalProvider,
        gate: Arc<Gate>,
    }

    impl StorageProvider for GatedProvider {
        fn name(&self) -> &'static str {
            "gated"
        }

        fn list(&self, dir: &Path) -> anyhow::Result<Vec<FsEntry>> {
            self.local.list(dir)
        }

        fn stat(&self, path: &Path) -> anyhow::Result<FsEntry> {
            self.local.stat(path)
        }

        fn create_dir(&self, path: &Path) -> anyhow::Result<()> {
            self.local.create_dir(path)
        }

        fn create_file(&self, path: &Path) -> anyhow::Result<()> {
            self.local.create_file(path)
        }

        fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()> {
            self.local.rename(from, to)
        }

        fn copy_file(
            &self,
            from: &Path,
            to: &Path,
            progress: ProgressFn,
            cancel: &AtomicBool,
            pause: Option<&PauseGate>,
        ) -> anyhow::Result<u64> {
            self.gate.seen.lock().push(from.to_path_buf());
            self.gate.entered.fetch_add(1, Ordering::SeqCst);
            while !self.gate.open.load(Ordering::SeqCst) && !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let pause = pause.filter(|_| self.gate.suspendable.load(Ordering::SeqCst));
            self.local.copy_file(from, to, progress, cancel, pause)
        }

        fn delete_to_trash(&self, paths: &[PathBuf]) -> anyhow::Result<()> {
            self.local.delete_to_trash(paths)
        }

        fn remove_after_move(&self, path: &Path) -> anyhow::Result<()> {
            self.local.remove_after_move(path)
        }
    }

    /// A scheduler plus the temp tree it works in, shut down on drop so the
    /// shared runtime does not accumulate a tick loop per test.
    struct Harness {
        rt: &'static BackendRuntime,
        scheduler: Scheduler,
        gate: Arc<Gate>,
        root: PathBuf,
    }

    impl Harness {
        fn new(concurrency: usize) -> Option<Self> {
            let rt = runtime::get()?;
            let gate = Gate::new();
            let provider = Arc::new(GatedProvider {
                local: LocalProvider::new(),
                gate: Arc::clone(&gate),
            });
            let root = std::env::temp_dir().join(format!(
                "piku-sched-{}-{}",
                std::process::id(),
                JobId::next()
            ));
            std::fs::create_dir_all(&root).ok()?;

            Some(Self {
                rt,
                scheduler: Scheduler::with_concurrency(
                    rt,
                    PathPolicy::with_system_roots(),
                    provider,
                    // A transfer test has no business exercising an indexer.
                    Pipeline::inert(),
                    concurrency,
                ),
                gate,
                root,
            })
        }

        /// A directory holding `names`, each one byte per character of its name.
        fn tree(&self, dir: &str, names: &[&str]) -> PathBuf {
            let path = self.root.join(dir);
            std::fs::create_dir_all(&path).expect("test tree");
            for name in names {
                std::fs::write(path.join(name), name.as_bytes()).expect("test file");
            }
            path
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.root.join(name);
            std::fs::create_dir_all(&path).expect("test dir");
            path
        }

        fn state(&self, id: JobId) -> Option<TransferState> {
            self.scheduler
                .states()
                .into_iter()
                .find(|(found, _)| *found == id)
                .map(|(_, state)| state)
        }

        /// Every live job's label, for an assertion message that says what the
        /// scheduler actually thought.
        fn states(&self) -> Vec<(u64, &'static str)> {
            self.scheduler
                .states()
                .into_iter()
                .map(|(id, state)| (id.0, state.label()))
                .collect()
        }

        fn label(&self, id: JobId) -> &'static str {
            self.state(id).map_or("gone", |state| state.label())
        }

        /// Spin until `predicate` holds. Returns whether it did.
        fn eventually(&self, predicate: impl Fn() -> bool) -> bool {
            let deadline = Instant::now() + DEADLINE;
            while Instant::now() < deadline {
                if predicate() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            false
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            self.gate.open();
            self.scheduler.shutdown();
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn copy(sources: &[PathBuf], destination: &Path, priority: Priority) -> TransferRequest {
        let mut request = TransferRequest::copy(
            sources.to_vec(),
            destination.to_path_buf(),
            ConflictPolicy::Replace,
        );
        request.priority = priority;
        request
    }

    /// The plan's first scheduler requirement: the permit pool is a real bound,
    /// not a hint.
    #[test]
    fn three_interactive_jobs_at_concurrency_one_run_one_at_a_time() {
        let Some(harness) = Harness::new(1) else {
            return; // no runtime in this environment
        };

        let ids: Vec<JobId> = (0..3)
            .map(|n| {
                let source = harness.tree(&format!("src{n}"), &["a", "b"]);
                let destination = harness.dir(&format!("dst{n}"));
                harness
                    .scheduler
                    .submit(copy(&[source], &destination, Priority::Interactive))
            })
            .collect();

        assert!(
            harness.eventually(|| harness.gate.entered() > 0),
            "no job ever reached the provider"
        );
        // Held at the gate, so the state cannot move underneath the assertion.
        assert_eq!(
            harness.gate.entered(),
            1,
            "more than one job moved bytes at concurrency 1"
        );

        let running = ids
            .iter()
            .filter(|id| harness.label(**id) == "running")
            .count();
        let queued = ids
            .iter()
            .filter(|id| harness.label(**id) == "queued")
            .count();
        assert_eq!(
            (running, queued),
            (1, 2),
            "states were {:?}",
            harness.states()
        );

        harness.gate.open();
        assert!(
            harness.eventually(|| ids.iter().all(|id| harness.state(*id).is_none())),
            "the queue did not drain: {:?}",
            harness.states()
        );
    }

    /// The plan's second requirement. Priority is re-read when a permit appears,
    /// so submission order does not decide it.
    #[test]
    fn a_background_job_never_preempts_an_interactive_one() {
        let Some(harness) = Harness::new(1) else {
            return;
        };

        // A hog to hold the only permit while the other two queue up behind it.
        let hog_source = harness.tree("hog", &["h"]);
        let hog = harness.scheduler.submit(copy(
            &[hog_source],
            &harness.dir("hog-dst"),
            Priority::Normal,
        ));
        assert!(harness.eventually(|| harness.gate.entered() == 1));

        // Background first, so FIFO alone would run it first.
        let background_source = harness.tree("bg", &["background-file"]);
        let background = harness.scheduler.submit(copy(
            &[background_source],
            &harness.dir("bg-dst"),
            Priority::Background,
        ));
        let urgent_source = harness.tree("ui", &["interactive-file"]);
        let urgent = harness.scheduler.submit(copy(
            &[urgent_source],
            &harness.dir("ui-dst"),
            Priority::Interactive,
        ));
        assert!(
            harness.eventually(|| {
                harness.label(background) == "queued" && harness.label(urgent) == "queued"
            }),
            "both should be waiting behind the hog"
        );

        harness.gate.open();
        assert!(
            harness.eventually(|| harness.state(hog).is_none()
                && harness.state(background).is_none()
                && harness.state(urgent).is_none()),
            "jobs did not drain: {:?}",
            harness.states()
        );

        let order: Vec<String> = harness
            .gate
            .seen()
            .iter()
            .filter_map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .collect();
        let urgent_at = order.iter().position(|name| name == "interactive-file");
        let background_at = order.iter().position(|name| name == "background-file");
        assert!(urgent_at < background_at, "background ran first: {order:?}");
    }

    /// A pause that lands at a file boundary has to free capacity, or "Pause
    /// All" would stop four jobs and start none.
    #[test]
    fn a_paused_job_hands_its_permit_to_a_queued_one() {
        let Some(harness) = Harness::new(1) else {
            return;
        };
        // No mid-file suspension point, so the pass's own loop head is the only
        // place the pause can be honoured — which is the file boundary this test
        // is about. The other arrangement is the next test.
        harness.gate.suspendable.store(false, Ordering::SeqCst);

        // Two files, so there is a boundary between them to stop at.
        let first_source = harness.tree("first", &["one", "two"]);
        let first = harness.scheduler.submit(copy(
            &[first_source],
            &harness.dir("first-dst"),
            Priority::Normal,
        ));
        assert!(harness.eventually(|| harness.gate.entered() == 1));

        let second_source = harness.tree("second", &["only"]);
        let second = harness.scheduler.submit(copy(
            &[second_source],
            &harness.dir("second-dst"),
            Priority::Normal,
        ));
        assert!(harness.eventually(|| harness.label(second) == "queued"));

        assert!(harness.scheduler.pause(first), "pause was refused");
        harness.gate.open();

        assert!(
            harness.eventually(|| harness.label(first) == "paused"),
            "first job never parked: {:?}",
            harness.states()
        );
        assert!(
            harness.eventually(|| harness.state(second).is_none()),
            "the queued job never got the permit: {:?}",
            harness.states()
        );

        assert!(harness.scheduler.resume(first), "resume was refused");
        assert!(
            harness.eventually(|| harness.state(first).is_none()),
            "the resumed job never finished: {:?}",
            harness.states()
        );
    }

    /// The other half of the pause story, and the one a later refactor would
    /// "fix" by throwing the partial write away: a pause that lands part-way
    /// through a file *keeps* its permit. `copy_file` parks between chunks with
    /// both handles open and the offset intact, and there is no way to hand that
    /// to another job.
    #[test]
    fn a_pause_inside_a_file_keeps_its_permit() {
        let Some(harness) = Harness::new(1) else {
            return;
        };

        let holder_source = harness.tree("holder", &["one", "two"]);
        let holder = harness.scheduler.submit(copy(
            &[holder_source],
            &harness.dir("holder-dst"),
            Priority::Normal,
        ));
        // Inside `copy_file`, held at the door.
        assert!(harness.eventually(|| harness.gate.entered() == 1));

        let queued_source = harness.tree("queued", &["only"]);
        let queued = harness.scheduler.submit(copy(
            &[queued_source],
            &harness.dir("queued-dst"),
            Priority::Normal,
        ));
        assert!(harness.eventually(|| harness.label(queued) == "queued"));

        // Requested while the provider is already in the file, so the chunk loop
        // is the suspension point it reaches — not the pass's boundary.
        assert!(harness.scheduler.pause(holder), "pause was refused");
        harness.gate.open();

        assert!(
            harness.eventually(|| harness.label(holder) == "paused"),
            "the tick never settled the parked worker: {:?}",
            harness.states()
        );

        // Several ticks' worth of chances to hand the permit over. It must not.
        for _ in 0..3 {
            std::thread::sleep(TICK);
            assert_eq!(
                harness.label(queued),
                "queued",
                "a mid-file pause gave up its permit: {:?}",
                harness.states()
            );
        }

        // And the park is a park, not a loss: resuming finishes both jobs.
        assert!(harness.scheduler.resume(holder), "resume was refused");
        assert!(
            harness
                .eventually(|| harness.state(holder).is_none() && harness.state(queued).is_none()),
            "nothing drained after the resume: {:?}",
            harness.states()
        );
    }

    /// Cancelling something that never started must be terminal immediately —
    /// there is no worker to reach a boundary and claim it.
    #[test]
    fn cancelling_a_queued_job_never_starts_it() {
        let Some(harness) = Harness::new(1) else {
            return;
        };

        let hog_source = harness.tree("hog", &["h"]);
        let hog = harness.scheduler.submit(copy(
            &[hog_source],
            &harness.dir("hog-dst"),
            Priority::Normal,
        ));
        assert!(harness.eventually(|| harness.gate.entered() == 1));

        let source = harness.tree("waiting", &["never-copied"]);
        let destination = harness.dir("waiting-dst");
        let queued = harness
            .scheduler
            .submit(copy(&[source], &destination, Priority::Normal));
        assert!(harness.eventually(|| harness.label(queued) == "queued"));

        assert!(harness.scheduler.cancel(queued));
        assert_eq!(harness.label(queued), "cancelled", "not terminal at once");
        assert!(!harness.scheduler.cancel(queued), "cancelled twice");

        harness.gate.open();
        assert!(harness.eventually(|| harness.state(hog).is_none()));

        assert!(
            !harness
                .gate
                .seen()
                .iter()
                .any(|path| path.ends_with("never-copied")),
            "a cancelled job still copied"
        );
        assert!(
            !destination.join("waiting").exists(),
            "a cancelled job wrote to its destination"
        );
    }

    #[test]
    fn cancelling_a_running_job_stops_it_at_the_next_boundary() {
        let Some(harness) = Harness::new(2) else {
            return;
        };

        let source = harness.tree("live", &["a", "b", "c"]);
        let id =
            harness
                .scheduler
                .submit(copy(&[source], &harness.dir("live-dst"), Priority::Normal));
        assert!(harness.eventually(|| harness.gate.entered() == 1));

        assert!(harness.scheduler.cancel(id));
        assert!(
            harness.eventually(|| harness.state(id).is_none()),
            "a cancelled job did not settle: {:?}",
            harness.states()
        );
    }

    /// The whole publish path in one test: a state edge, a periodic reading, and
    /// a terminal summary, all off the one tick.
    #[test]
    fn the_tick_publishes_states_readings_and_a_summary() {
        let Some(harness) = Harness::new(2) else {
            return;
        };
        let mut stream = harness.scheduler.subscribe();

        let source = harness.tree("published", &["x", "yy"]);
        let id = harness.scheduler.submit(copy(
            &[source],
            &harness.dir("published-dst"),
            Priority::Normal,
        ));
        harness.gate.open();

        let mut states = Vec::new();
        let mut readings = 0usize;
        let mut summary = None;

        // The stream is async, so the collect runs on the runtime — from this
        // thread, which is not one of its workers, so the scheduler's own tasks
        // keep making progress while this one waits.
        harness.rt.handle().block_on(async {
            while summary.is_none() {
                let Ok(Some(item)) = tokio::time::timeout(DEADLINE, stream.next()).await else {
                    break;
                };
                let StreamItem::Batch(events) = item else {
                    continue;
                };
                for event in events {
                    match event {
                        TransferEvent::Job {
                            update: TransferUpdate::State(state),
                            ..
                        } => states.push(state.label()),
                        TransferEvent::Tick(samples) => {
                            readings += samples.iter().filter(|s| s.id == id).count();
                        }
                        TransferEvent::Finished { summary: done, .. } => summary = Some(done),
                        TransferEvent::Job { .. } => {}
                    }
                }
            }
        });

        let summary = summary.expect("no summary was published");
        // The created directory counts as an item, so two files make three.
        assert_eq!(summary.items, 3, "summary counted {} items", summary.items);
        assert_eq!(summary.bytes, 3, "one byte per character of each name");
        assert!(readings > 0, "no periodic reading mentioned the job");
        assert!(
            states.contains(&"completed"),
            "terminal state never published: {states:?}"
        );
        assert!(
            harness.eventually(|| harness.state(id).is_none()),
            "a finished job stayed in the registry"
        );
    }

    /// A collision with no policy stops the job dead, and only an answer moves
    /// it. This is the `WaitingForInput` half of the state machine end to end.
    #[test]
    fn a_collision_with_no_policy_waits_for_an_answer() {
        let Some(harness) = Harness::new(2) else {
            return;
        };

        let source = harness.tree("clash-src", &["taken"]);
        let destination = harness.dir("clash-dst");
        std::fs::create_dir_all(destination.join("clash-src")).expect("existing tree");
        std::fs::write(destination.join("clash-src").join("taken"), b"old").expect("existing file");

        let request = TransferRequest::copy(vec![source], destination.clone(), ConflictPolicy::Ask);
        let id = harness.scheduler.submit(request);
        harness.gate.open();

        assert!(
            harness.eventually(|| harness.label(id) == "waiting"),
            "the job did not stop for the collision: {:?}",
            harness.states()
        );
        // And it stays there: nothing but a decision can move it.
        std::thread::sleep(TICK * 3);
        assert_eq!(harness.label(id), "waiting");

        assert!(
            harness
                .scheduler
                .resolve(id, Decision::All(ConflictPolicy::Replace)),
            "the decision was not delivered"
        );
        assert!(
            harness.eventually(|| harness.state(id).is_none()),
            "the answered job never finished: {:?}",
            harness.states()
        );
        assert_eq!(
            std::fs::read(destination.join("clash-src").join("taken")).expect("replaced file"),
            b"taken",
            "the source did not replace the existing file"
        );
    }

    /// Shutdown is the plan's third scheduler requirement, restated for the
    /// mechanism that actually exists: the flag, not `abort_all`.
    #[test]
    fn shutdown_cancels_every_live_job() {
        let Some(harness) = Harness::new(2) else {
            return;
        };

        let ids: Vec<JobId> = (0..3)
            .map(|n| {
                let source = harness.tree(&format!("bulk{n}"), &["a"]);
                let destination = harness.dir(&format!("bulk-dst{n}"));
                harness
                    .scheduler
                    .submit(copy(&[source], &destination, Priority::Normal))
            })
            .collect();
        assert!(harness.eventually(|| harness.gate.entered() > 0));

        harness.scheduler.shutdown();

        // Every control is cancelled synchronously, whatever state it was in.
        assert!(
            ids.iter().all(|id| {
                harness
                    .state(*id)
                    .is_none_or(|state| !matches!(state, TransferState::Completed))
            }),
            "shutdown let a job complete: {:?}",
            harness.states()
        );
        // And the loops are gone, so nothing new is admitted.
        let after = harness.scheduler.submit(copy(
            &[harness.tree("late", &["a"])],
            &harness.dir("late-dst"),
            Priority::Interactive,
        ));
        std::thread::sleep(TICK * 3);
        assert_eq!(
            harness.label(after),
            "queued",
            "a job was admitted after shutdown"
        );
    }

    #[test]
    fn a_job_that_does_not_exist_refuses_every_operation() {
        let Some(harness) = Harness::new(1) else {
            return;
        };
        let absent = JobId::next();
        assert!(!harness.scheduler.pause(absent));
        assert!(!harness.scheduler.resume(absent));
        assert!(!harness.scheduler.cancel(absent));
        assert!(!harness.scheduler.prioritize(absent, Priority::Interactive));
        assert!(!harness.scheduler.resolve(absent, Decision::Cancel));
        assert!(harness.scheduler.states().is_empty());
    }

    #[test]
    fn audit_op_names_fit_the_record() {
        for op in [AUDIT_PAUSE, AUDIT_RESUME, AUDIT_CANCEL] {
            assert!(op.len() <= 64, "`{op}` exceeds MAX_OP_CHARS");
            assert!(
                op.starts_with("transfer."),
                "`{op}` is not namespaced to the engine"
            );
        }
    }
}
