//! The storage abstraction every UI operation goes through. Implementations
//! are synchronous and blocking; callers run them on the background executor
//! so the render thread never touches storage.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::core::entry::FsEntry;

/// Byte-level progress callback used during long copies.
pub type ProgressFn<'a> = &'a mut dyn FnMut(u64);

/// How long a parked worker sleeps before re-checking cancellation.
///
/// A backstop, not the mechanism: [`PauseGate::wake`] is what normally wakes a
/// parked copy. But cancellation sets an `AtomicBool` this gate does not own, so
/// a caller that sets the flag without calling `wake` would otherwise leave a
/// worker asleep forever. Four wakeups a second while paused costs nothing.
const PARK_POLL: Duration = Duration::from_millis(250);

/// A suspension point a long copy can park on.
///
/// # Why this is not cancellation
///
/// Cancelling a copy deletes the partial destination file and gives up. Pausing
/// it parks the loop **between chunks with both file handles open**, so the read
/// offset, the write offset, and the partial file all survive. That is what lets
/// resume be free: there is no checkpoint to write and no partial-file
/// bookkeeping to get wrong.
///
/// # Why it lives here
///
/// So `storage` can *be* suspended without depending on whatever is doing the
/// suspending. The transfer engine owns one per job and hands out a reference;
/// every other caller passes `None`.
///
/// # The three flags
///
/// `paused` under the mutex is the truth. `requested` mirrors it lock-free, so
/// the per-chunk check in the copy loop costs one atomic load rather than a lock
/// acquisition. `parked` records whether a worker has actually stopped, which is
/// the difference between reporting `Pausing` and reporting `Paused` — a UI that
/// claims `Paused` while a 1 MB write is still draining is lying.
pub struct PauseGate {
    requested: AtomicBool,
    paused: Mutex<bool>,
    resumed: Condvar,
    parked: AtomicBool,
}

impl Default for PauseGate {
    fn default() -> Self {
        Self::new()
    }
}

impl PauseGate {
    pub fn new() -> Self {
        Self {
            requested: AtomicBool::new(false),
            paused: Mutex::new(false),
            resumed: Condvar::new(),
            parked: AtomicBool::new(false),
        }
    }

    pub fn request_pause(&self) {
        let mut paused = self.paused.lock();
        *paused = true;
        self.requested.store(true, Ordering::Release);
    }

    pub fn request_resume(&self) {
        {
            let mut paused = self.paused.lock();
            *paused = false;
            self.requested.store(false, Ordering::Release);
        }
        self.resumed.notify_all();
    }

    /// Whether a pause has been asked for. Cheap enough for a per-chunk check.
    pub fn is_pause_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    /// Whether a worker is parked *right now*.
    pub fn is_parked(&self) -> bool {
        self.parked.load(Ordering::Acquire)
    }

    /// Wake a parked worker without lifting the pause.
    ///
    /// Cancellation calls this. The lock is taken before notifying so the wakeup
    /// cannot slip into the window between a waiter re-checking the predicate and
    /// registering itself.
    pub fn wake(&self) {
        let _held = self.paused.lock();
        self.resumed.notify_all();
    }

    /// Park while paused. Returns `false` if the operation was cancelled —
    /// either before parking or while parked.
    ///
    /// The predicate is re-checked under the mutex on every wakeup, so a spurious
    /// one costs a loop iteration rather than a resumed-too-early copy.
    pub fn wait_while_paused(&self, cancel: &AtomicBool) -> bool {
        if !self.requested.load(Ordering::Acquire) {
            return !cancel.load(Ordering::Acquire);
        }

        let mut paused = self.paused.lock();
        self.parked.store(true, Ordering::Release);
        while *paused && !cancel.load(Ordering::Acquire) {
            self.resumed.wait_for(&mut paused, PARK_POLL);
        }
        self.parked.store(false, Ordering::Release);
        drop(paused);

        !cancel.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for PauseGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PauseGate")
            .field("requested", &self.is_pause_requested())
            .field("parked", &self.is_parked())
            .finish()
    }
}

pub trait StorageProvider: Send + Sync + 'static {
    #[allow(dead_code)]
    fn name(&self) -> &'static str;

    /// List the immediate children of a directory.
    fn list(&self, dir: &Path) -> anyhow::Result<Vec<FsEntry>>;

    /// Metadata for a single path.
    #[allow(dead_code)]
    fn stat(&self, path: &Path) -> anyhow::Result<FsEntry>;

    fn create_dir(&self, path: &Path) -> anyhow::Result<()>;

    /// Create a new empty file; fails if the path already exists.
    fn create_file(&self, path: &Path) -> anyhow::Result<()>;

    fn rename(&self, from: &Path, to: &Path) -> anyhow::Result<()>;

    /// Copy one file, reporting bytes copied and honouring cancellation.
    ///
    /// `pause`, when supplied, is checked once per chunk: the copy parks with
    /// both handles open rather than unwinding, so resuming needs no checkpoint.
    /// Callers with nothing to suspend pass `None`.
    fn copy_file(
        &self,
        from: &Path,
        to: &Path,
        progress: ProgressFn,
        cancel: &AtomicBool,
        pause: Option<&PauseGate>,
    ) -> anyhow::Result<u64>;

    /// Move deleted items to the OS trash. This is what the Delete key does, and
    /// it is reversible. Permanent deletion is a separate, explicitly chosen
    /// operation that goes through the transfer engine — never a fallback for
    /// this one failing.
    fn delete_to_trash(&self, paths: &[PathBuf]) -> anyhow::Result<()>;

    /// Remove a file or empty directory permanently. Used by the transfer engine
    /// to clear sources after a verified move, and to carry out an explicit
    /// permanent delete.
    fn remove_after_move(&self, path: &Path) -> anyhow::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::thread;

    /// Spin until `predicate` holds, up to a generous bound. Returns whether it
    /// held — no `sleep`-and-assume, which is how a thread test becomes flaky on
    /// a loaded CI box.
    fn eventually(predicate: impl Fn() -> bool) -> bool {
        for _ in 0..2_000 {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(1));
        }
        false
    }

    #[test]
    fn an_unpaused_gate_never_blocks() {
        let gate = PauseGate::new();
        let cancel = AtomicBool::new(false);
        assert!(!gate.is_pause_requested());
        assert!(!gate.is_parked());
        assert!(gate.wait_while_paused(&cancel));
    }

    #[test]
    fn a_cancelled_operation_is_reported_even_without_a_pause() {
        let gate = PauseGate::new();
        let cancel = AtomicBool::new(true);
        assert!(!gate.wait_while_paused(&cancel));
    }

    /// The core behavior: a worker parks, stays parked, and continues on resume.
    #[test]
    fn a_worker_parks_until_it_is_resumed() {
        let gate = Arc::new(PauseGate::new());
        let cancel = Arc::new(AtomicBool::new(false));
        let chunks = Arc::new(AtomicU64::new(0));

        gate.request_pause();

        let worker = {
            let (gate, cancel, chunks) = (gate.clone(), cancel.clone(), chunks.clone());
            thread::spawn(move || {
                let running = gate.wait_while_paused(&cancel);
                chunks.fetch_add(1, Ordering::SeqCst);
                running
            })
        };

        assert!(eventually(|| gate.is_parked()), "the worker never parked");
        // And it stays parked: nothing has resumed it.
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            chunks.load(Ordering::SeqCst),
            0,
            "the worker moved while paused"
        );
        assert!(gate.is_parked());

        gate.request_resume();
        assert!(worker.join().unwrap(), "resume reported cancellation");
        assert_eq!(chunks.load(Ordering::SeqCst), 1);
        assert!(!gate.is_parked());
        assert!(!gate.is_pause_requested());
    }

    /// Cancelling a *paused* job must not wait for the poll timeout, and must
    /// report cancellation rather than a clean resume.
    #[test]
    fn cancelling_a_parked_worker_wakes_it_immediately() {
        let gate = Arc::new(PauseGate::new());
        let cancel = Arc::new(AtomicBool::new(false));
        gate.request_pause();

        let worker = {
            let (gate, cancel) = (gate.clone(), cancel.clone());
            thread::spawn(move || gate.wait_while_paused(&cancel))
        };
        assert!(eventually(|| gate.is_parked()));

        cancel.store(true, Ordering::SeqCst);
        gate.wake();

        assert!(!worker.join().unwrap(), "a cancelled park reported success");
        // The pause request is still standing — cancel does not lift it, it
        // overrides it.
        assert!(gate.is_pause_requested());
    }

    /// The poll backstop: a caller that sets the cancel flag and forgets to call
    /// `wake` must not leave a worker asleep forever.
    #[test]
    fn a_forgotten_wake_still_resolves_through_the_poll() {
        let gate = Arc::new(PauseGate::new());
        let cancel = Arc::new(AtomicBool::new(false));
        gate.request_pause();

        let worker = {
            let (gate, cancel) = (gate.clone(), cancel.clone());
            thread::spawn(move || gate.wait_while_paused(&cancel))
        };
        assert!(eventually(|| gate.is_parked()));

        cancel.store(true, Ordering::SeqCst); // no `wake`
        assert!(!worker.join().unwrap());
    }

    /// `Pausing` versus `Paused` is exactly this distinction, so the two flags
    /// must be able to disagree.
    #[test]
    fn requesting_a_pause_is_not_the_same_as_being_parked() {
        let gate = PauseGate::new();
        gate.request_pause();
        assert!(gate.is_pause_requested(), "the request is in");
        assert!(!gate.is_parked(), "but nothing has stopped yet");
    }

    #[test]
    fn pausing_and_resuming_repeatedly_leaves_no_residue() {
        let gate = Arc::new(PauseGate::new());
        let cancel = Arc::new(AtomicBool::new(false));

        for _ in 0..20 {
            gate.request_pause();
            let worker = {
                let (gate, cancel) = (gate.clone(), cancel.clone());
                thread::spawn(move || gate.wait_while_paused(&cancel))
            };
            assert!(eventually(|| gate.is_parked()));
            gate.request_resume();
            assert!(worker.join().unwrap());
        }
        assert!(!gate.is_parked());
        assert!(!gate.is_pause_requested());
        assert!(gate.wait_while_paused(&cancel));
    }

    /// A pause that arrives and is lifted before the worker ever looks must not
    /// leave the worker parked — the mirror flag and the mutex have to agree.
    #[test]
    fn a_pause_lifted_before_anyone_parks_is_a_no_op() {
        let gate = PauseGate::new();
        let cancel = AtomicBool::new(false);
        gate.request_pause();
        gate.request_resume();
        assert!(gate.wait_while_paused(&cancel));
        assert!(!gate.is_parked());
    }
}
