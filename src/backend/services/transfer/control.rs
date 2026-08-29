//! [`TransferControl`]: the one thing the UI, the sample tick, and the blocking
//! copy loop all hold a reference to.
//!
//! # Why a shared cell and not a channel
//!
//! The obvious design has the copy loop send a progress message per chunk. At a
//! 1 MB chunk and 500 MB/s that is 500 messages a second per job, every one of
//! which the UI thread has to receive, apply, and coalesce — so the faster the
//! disk, the more of the render loop the transfer eats.
//!
//! Instead the copy loop bumps atomics here, and the scheduler's single 200 ms
//! tick reads them and publishes. Cost per chunk: one relaxed `fetch_add`.
//! Publishing rate: fixed, and independent of how fast the disk is.
//!
//! # Locks and awaits
//!
//! Every method locks and releases before returning. Nothing hands out a guard,
//! because `clippy::await_holding_lock` is denied crate-wide and a
//! `parking_lot` guard held across an `.await` is exactly the deadlock that lint
//! exists to prevent. [`TransferControl::wait_for_resume`] is the only `async`
//! method, and it touches nothing but a `Notify`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

use crate::backend::protocol::Progress;
use crate::backend::services::transfer::conflict::Decision;
use crate::backend::services::transfer::job::{
    JobId, Priority, TransferFailure, TransferKind, TransferState,
};
use crate::storage::provider::PauseGate;

/// Upper bound on retained per-item failures.
///
/// A copy of 40 000 files across a failing disk must not turn into 40 000 heap
/// strings on the way to a list nobody can read. The true count is kept
/// separately, so the card can say "and 39 744 more" honestly.
const MAX_KEPT_FAILURES: usize = 256;

/// Shared, lock-light state for one job.
///
/// Held as an `Arc` by the service (so the UI can pause it), by the scheduler
/// (so the tick can read it), and by the blocking copy closure (so it can bump
/// counters and park).
pub struct TransferControl {
    id: JobId,
    kind: TransferKind,
    priority: Mutex<Priority>,

    /// The flag `StorageProvider::copy_file` already takes. Shared rather than
    /// mirrored so there is exactly one answer to "was this cancelled".
    cancel: Arc<AtomicBool>,
    pause: PauseGate,
    /// Wakes the async job driver from a between-files pause. Separate from the
    /// gate's condvar because that one parks an OS thread and this one parks a
    /// task.
    resume: Notify,

    done_bytes: AtomicU64,
    total_bytes: AtomicU64,
    done_items: AtomicU64,
    total_items: AtomicU64,
    skipped: AtomicU64,
    replaced: AtomicU64,

    state: Mutex<TransferState>,
    /// Bumped on every state write. The publisher compares it against what it
    /// last sent, which is how "send only on change" works without `PartialEq`
    /// on a state that contains an `io::Error`.
    revision: AtomicU64,

    /// Sanitized display name of the item in flight. Never a path.
    current: Mutex<Option<Arc<str>>>,

    failures: Mutex<Vec<TransferFailure>>,
    failure_count: AtomicU64,
    /// How many entries of `failures` the tick has already published.
    published_failures: AtomicUsize,

    decision: Mutex<Option<oneshot::Sender<Decision>>>,
}

impl TransferControl {
    pub fn new(id: JobId, kind: TransferKind, priority: Priority) -> Arc<Self> {
        Arc::new(Self {
            id,
            kind,
            priority: Mutex::new(priority),
            cancel: Arc::new(AtomicBool::new(false)),
            pause: PauseGate::new(),
            resume: Notify::new(),
            done_bytes: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            done_items: AtomicU64::new(0),
            total_items: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            replaced: AtomicU64::new(0),
            state: Mutex::new(TransferState::Queued),
            revision: AtomicU64::new(0),
            current: Mutex::new(None),
            failures: Mutex::new(Vec::new()),
            failure_count: AtomicU64::new(0),
            published_failures: AtomicUsize::new(0),
            decision: Mutex::new(None),
        })
    }

    pub fn id(&self) -> JobId {
        self.id
    }

    pub fn kind(&self) -> TransferKind {
        self.kind
    }

    pub fn priority(&self) -> Priority {
        *self.priority.lock()
    }

    /// Raise or lower the job's class. Only takes effect while it is queued —
    /// a running job is already running.
    pub fn set_priority(&self, priority: Priority) {
        *self.priority.lock() = priority;
    }

    // --- cancellation -----------------------------------------------------

    /// The flag to hand to `copy_file`.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    /// Stop the job as soon as it reaches its next chunk boundary.
    ///
    /// Wakes everything that could be asleep, and drops any armed decision
    /// channel so a job stopped on a conflict does not sit there waiting for an
    /// answer that will never come.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        // A parked worker is blocked on the gate's condvar, which knows nothing
        // about the flag above.
        self.pause.wake();
        self.resume.notify_one();
        drop(self.decision.lock().take());
    }

    // --- pause ------------------------------------------------------------

    pub fn pause_gate(&self) -> &PauseGate {
        &self.pause
    }

    pub fn is_pause_requested(&self) -> bool {
        self.pause.is_pause_requested()
    }

    /// Ask the job to stop at its next safe point.
    ///
    /// Moves a running job to `Pausing`, not `Paused`: the request is in, but a
    /// 1 MB write may still be draining, and claiming otherwise is a lie the
    /// user can see (the byte counter keeps moving).
    pub fn request_pause(&self) {
        self.pause.request_pause();
        let mut state = self.state.lock();
        if matches!(*state, TransferState::Running) {
            *state = TransferState::Pausing;
            drop(state);
            self.bump();
        }
    }

    pub fn request_resume(&self) {
        self.pause.request_resume();
        {
            let mut state = self.state.lock();
            if matches!(*state, TransferState::Pausing | TransferState::Paused) {
                *state = TransferState::Running;
            }
        }
        self.bump();
        self.resume.notify_one();
    }

    /// Promote `Pausing` to `Paused` once a worker has actually stopped.
    ///
    /// Called from the sample tick, because the thread that parked is blocked
    /// inside a condvar wait and has no way to publish anything.
    pub fn settle_pause_state(&self) {
        if !self.pause.is_parked() {
            return;
        }
        let mut state = self.state.lock();
        if matches!(*state, TransferState::Pausing) {
            *state = TransferState::Paused;
            drop(state);
            self.bump();
        }
    }

    /// Park the *task* until the pause is lifted or the job is cancelled.
    ///
    /// Used at a file boundary and before the first file, where the job driver
    /// can give its scheduler permit back — which is what lets "Pause All" free
    /// capacity for a queued job instead of merely stopping four of them.
    pub async fn wait_for_resume(&self) {
        while self.pause.is_pause_requested() && !self.is_cancelled() {
            // `notify_one` stores a permit when nobody is waiting, so a resume
            // that lands between the check above and this await is not lost.
            self.resume.notified().await;
        }
    }

    // --- progress ---------------------------------------------------------

    pub fn set_totals(&self, bytes: u64, items: u64) {
        self.total_bytes.store(bytes, Ordering::Relaxed);
        self.total_items.store(items, Ordering::Relaxed);
    }

    /// Called once per copied chunk. Deliberately the cheapest thing here.
    pub fn add_bytes(&self, bytes: u64) {
        self.done_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn add_items(&self, items: u64) {
        self.done_items.fetch_add(items, Ordering::Relaxed);
    }

    pub fn add_skipped(&self, items: u64) {
        self.skipped.fetch_add(items, Ordering::Relaxed);
    }

    pub fn add_replaced(&self, items: u64) {
        self.replaced.fetch_add(items, Ordering::Relaxed);
    }

    pub fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    pub fn replaced(&self) -> u64 {
        self.replaced.load(Ordering::Relaxed)
    }

    pub fn progress(&self) -> Progress {
        Progress {
            done_bytes: self.done_bytes.load(Ordering::Relaxed),
            total_bytes: self.total_bytes.load(Ordering::Relaxed),
            done_items: self.done_items.load(Ordering::Relaxed),
            total_items: self.total_items.load(Ordering::Relaxed),
        }
    }

    /// Bytes still to move, for the ETA. Saturating, because a scan that
    /// under-counted (a file grew mid-transfer) must not wrap.
    pub fn remaining_bytes(&self) -> u64 {
        self.total_bytes
            .load(Ordering::Relaxed)
            .saturating_sub(self.done_bytes.load(Ordering::Relaxed))
    }

    // --- state ------------------------------------------------------------

    pub fn state(&self) -> TransferState {
        self.state.lock().clone()
    }

    pub fn set_state(&self, next: TransferState) {
        *self.state.lock() = next;
        self.bump();
    }

    /// Monotonic counter over state writes; the publisher's change detector.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    // --- current item -----------------------------------------------------

    /// Record what is in flight.
    ///
    /// Takes a `Path` and yields display text, so the sanitizing happens in one
    /// place and a raw path physically cannot reach the UI through this field.
    /// The file name rather than the whole path: the card already shows the
    /// source and destination directories on its identity row.
    pub fn set_current(&self, path: Option<&Path>) {
        let text = path.and_then(|p| p.file_name()).map(|name| {
            let cleaned = crate::security::text::sanitize_label(&name.to_string_lossy());
            Arc::<str>::from(cleaned)
        });
        *self.current.lock() = text;
    }

    pub fn current(&self) -> Option<Arc<str>> {
        self.current.lock().clone()
    }

    // --- per-item failures ------------------------------------------------

    /// Record an item the job could not handle and carried on past.
    ///
    /// One locked file does not fail a 40 000-file copy; it becomes a row in the
    /// expanded card. Retained entries are capped, the count is not.
    pub fn push_failure(&self, path: &Path, detail: &str) {
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        let mut failures = self.failures.lock();
        if failures.len() >= MAX_KEPT_FAILURES {
            return;
        }
        failures.push(TransferFailure {
            path: crate::security::text::sanitize_path(path).into(),
            detail: crate::security::text::sanitize_label(detail).into(),
        });
    }

    /// The true number of failures, including those past the retention cap.
    pub fn failure_count(&self) -> u64 {
        self.failure_count.load(Ordering::Relaxed)
    }

    pub fn failures(&self) -> Vec<TransferFailure> {
        self.failures.lock().clone()
    }

    /// Failures the tick has not published yet, marking them published.
    pub fn take_new_failures(&self) -> Vec<TransferFailure> {
        let failures = self.failures.lock();
        let from = self.published_failures.load(Ordering::Relaxed);
        if from >= failures.len() {
            return Vec::new();
        }
        self.published_failures
            .store(failures.len(), Ordering::Relaxed);
        failures[from..].to_vec()
    }

    // --- decisions --------------------------------------------------------

    /// Install a fresh decision channel and hand back the receiver.
    ///
    /// One channel per ask: an answer to the previous question must not satisfy
    /// this one, so arming replaces any sender still sitting there.
    pub fn arm_decision(&self) -> oneshot::Receiver<Decision> {
        let (tx, rx) = oneshot::channel();
        *self.decision.lock() = Some(tx);
        rx
    }

    /// Deliver an answer. `false` if nothing was waiting for one — a stale
    /// click, or a job that was cancelled while the modal was open.
    pub fn deliver(&self, decision: Decision) -> bool {
        let sender = self.decision.lock().take();
        match sender {
            Some(tx) => tx.send(decision).is_ok(),
            None => false,
        }
    }

    /// Whether the job is currently stopped on a question.
    pub fn is_awaiting_decision(&self) -> bool {
        self.decision.lock().is_some()
    }
}

impl std::fmt::Debug for TransferControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransferControl")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("state", &self.state.lock().label())
            .field("cancelled", &self.is_cancelled())
            .field("paused", &self.pause)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::services::transfer::conflict::ConflictPolicy;
    use crate::backend::services::transfer::job::StateGroup;

    fn control() -> Arc<TransferControl> {
        TransferControl::new(JobId::next(), TransferKind::Copy, Priority::Normal)
    }

    #[test]
    fn a_fresh_control_is_queued_and_empty() {
        let control = control();
        assert!(matches!(control.state(), TransferState::Queued));
        assert_eq!(control.revision(), 0);
        assert_eq!(control.progress().percent(), 0.0);
        assert!(control.current().is_none());
        assert!(!control.is_cancelled());
        assert!(!control.is_pause_requested());
        assert!(!control.is_awaiting_decision());
    }

    #[test]
    fn the_revision_moves_only_on_a_state_write() {
        let control = control();
        let before = control.revision();

        // Progress is not a state change: this is what keeps a 500 MB/s copy
        // from publishing 500 times a second.
        control.add_bytes(1_000_000);
        control.add_items(1);
        control.set_current(Some(Path::new("/tmp/a.txt")));
        assert_eq!(control.revision(), before, "progress bumped the revision");

        control.set_state(TransferState::Running);
        assert!(control.revision() > before);
    }

    /// Pausing a running job must go through `Pausing`. The card's status text
    /// reads straight off this, so skipping it means claiming the disk has
    /// stopped while it is still writing.
    #[test]
    fn pausing_passes_through_pausing_and_settles_on_paused() {
        let control = control();
        control.set_state(TransferState::Running);

        control.request_pause();
        assert!(matches!(control.state(), TransferState::Pausing));
        assert!(control.is_pause_requested());

        // The tick cannot settle it while nothing has actually parked.
        control.settle_pause_state();
        assert!(
            matches!(control.state(), TransferState::Pausing),
            "settled without a parked worker"
        );

        // Park a worker the way `copy_file` does, then settle.
        let cancel = control.cancel_flag();
        let gate_owner = control.clone();
        let worker = std::thread::spawn(move || gate_owner.pause_gate().wait_while_paused(&cancel));
        for _ in 0..2_000 {
            if control.pause_gate().is_parked() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(control.pause_gate().is_parked(), "worker never parked");

        control.settle_pause_state();
        assert!(matches!(control.state(), TransferState::Paused));
        assert_eq!(control.state().group(), StateGroup::Active);

        control.request_resume();
        assert!(matches!(control.state(), TransferState::Running));
        assert!(worker.join().unwrap(), "resume reported cancellation");
    }

    #[test]
    fn cancelling_wakes_a_parked_worker_and_reports_cancellation() {
        let control = control();
        control.set_state(TransferState::Running);
        control.request_pause();

        let cancel = control.cancel_flag();
        let gate_owner = control.clone();
        let worker = std::thread::spawn(move || gate_owner.pause_gate().wait_while_paused(&cancel));
        for _ in 0..2_000 {
            if control.pause_gate().is_parked() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        control.cancel();
        assert!(!worker.join().unwrap());
        assert!(control.is_cancelled());
    }

    #[tokio::test]
    async fn a_resume_that_arrives_first_is_not_lost() {
        let control = control();
        control.set_state(TransferState::Running);
        control.request_pause();
        // Resume before anyone awaits. A `notify_waiters` here would be dropped
        // on the floor and the job would hang.
        control.request_resume();
        control.wait_for_resume().await;
        assert!(matches!(control.state(), TransferState::Running));
    }

    #[tokio::test]
    async fn cancelling_releases_a_task_parked_between_files() {
        let control = control();
        control.set_state(TransferState::Running);
        control.request_pause();

        let waiter = control.clone();
        let handle = tokio::spawn(async move { waiter.wait_for_resume().await });
        tokio::task::yield_now().await;
        control.cancel();
        handle.await.expect("the parked task never woke");
        assert!(control.is_pause_requested(), "cancel lifted the pause");
    }

    #[test]
    fn a_decision_reaches_exactly_one_waiter() {
        let control = control();
        let rx = control.arm_decision();
        assert!(control.is_awaiting_decision());

        assert!(control.deliver(Decision::All(ConflictPolicy::Replace)));
        assert!(!control.is_awaiting_decision());
        assert_eq!(
            rx.blocking_recv().ok(),
            Some(Decision::All(ConflictPolicy::Replace))
        );

        // A second click has nothing to answer.
        assert!(!control.deliver(Decision::Cancel));
    }

    /// Re-arming must invalidate the old channel, or an answer to the previous
    /// question satisfies the current one.
    #[test]
    fn re_arming_abandons_the_previous_question() {
        let control = control();
        let stale = control.arm_decision();
        let fresh = control.arm_decision();

        assert!(stale.blocking_recv().is_err(), "the stale channel survived");
        assert!(control.deliver(Decision::Proceed));
        assert_eq!(fresh.blocking_recv().ok(), Some(Decision::Proceed));
    }

    #[test]
    fn cancelling_abandons_an_open_question() {
        let control = control();
        let rx = control.arm_decision();
        control.cancel();
        assert!(
            rx.blocking_recv().is_err(),
            "a cancelled job still waits for an answer"
        );
        assert!(!control.is_awaiting_decision());
    }

    #[test]
    fn the_current_item_is_a_sanitized_name_not_a_path() {
        let control = control();
        // A filename carrying a right-to-left override must not survive.
        control.set_current(Some(Path::new("/home/u/dir/in\u{202E}gpj.exe")));
        let current = control.current().expect("a current item");
        assert_eq!(&*current, "ingpj.exe");
        assert!(!current.contains('/'), "a path reached the display field");

        control.set_current(None);
        assert!(control.current().is_none());
    }

    #[test]
    fn failures_are_capped_but_the_count_is_not() {
        let control = control();
        for i in 0..(MAX_KEPT_FAILURES + 50) {
            control.push_failure(
                &Path::new("/tmp").join(format!("f{i}")),
                "permission denied",
            );
        }
        assert_eq!(control.failures().len(), MAX_KEPT_FAILURES);
        assert_eq!(control.failure_count(), (MAX_KEPT_FAILURES + 50) as u64);
    }

    #[test]
    fn each_failure_is_published_exactly_once() {
        let control = control();
        assert!(control.take_new_failures().is_empty());

        control.push_failure(Path::new("/tmp/a"), "locked");
        control.push_failure(Path::new("/tmp/b"), "locked");
        let first = control.take_new_failures();
        assert_eq!(first.len(), 2);
        assert!(
            control.take_new_failures().is_empty(),
            "a failure was published twice"
        );

        control.push_failure(Path::new("/tmp/c"), "locked");
        let second = control.take_new_failures();
        assert_eq!(second.len(), 1);
        assert_eq!(&*second[0].path, "/tmp/c");
    }

    #[test]
    fn remaining_bytes_do_not_wrap_when_the_scan_undercounted() {
        let control = control();
        control.set_totals(100, 1);
        control.add_bytes(250); // the file grew after the scan
        assert_eq!(control.remaining_bytes(), 0);
    }

    #[test]
    fn priority_can_be_raised_after_submission() {
        let control = control();
        assert_eq!(control.priority(), Priority::Normal);
        control.set_priority(Priority::Interactive);
        assert_eq!(control.priority(), Priority::Interactive);
    }
}
