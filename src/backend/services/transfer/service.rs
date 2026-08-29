//! The handle the UI holds.
//!
//! # What this adds over the scheduler
//!
//! The [`Scheduler`] is the engine; this is the only door into it. Three things
//! live here and nowhere else:
//!
//! * **The collaborators.** Which [`StorageProvider`], which [`PathPolicy`], and
//!   which [`Pipeline`] are decisions made once, at construction, by the process
//!   that owns the backend. A caller cannot accidentally build a second
//!   scheduler with a policy rooted somewhere else, because the constructor that
//!   takes those arguments is not the one the UI can reach.
//! * **The structural gate.** [`submit`](TransferService::submit) refuses a
//!   request that cannot describe an operation — no sources, or a destination
//!   that does not match the kind — before a job id exists. That keeps a
//!   phantom card out of the transfer center and a meaningless line out of the
//!   audit log. It is deliberately *only* structural: nothing here touches the
//!   filesystem, and no path is authorized until
//!   [`precheck::approve`](super::precheck::approve) runs on a worker thread.
//!   One place decides, and it is not this one.
//! * **The verbs.** `copy`, `move_to`, and `delete_permanent` are what the UI
//!   actually means, so they are what it calls. Permanent delete having its own
//!   named method — rather than being a `TransferKind` a caller passes in — is
//!   the API-shaped half of "permanent deletion is a separate explicit
//!   operation, never a fallback for the trash".
//!
//! Everything else is delegation, on purpose. Pause, resume, cancel, resolve,
//! and prioritize are single field writes behind a mutex in
//! [`TransferControl`](super::control::TransferControl); wrapping them in a
//! [`BackendTask`](crate::backend::protocol::BackendTask) would add a round trip
//! to a runtime for the privilege of setting an `AtomicBool`.
//!
//! # Why `submit` is not async
//!
//! A paste is one of two interactions where the click and the effect have to feel
//! simultaneous — the other is a keystroke. So submission allocates an id, pushes
//! onto a deque, and returns; the card appears in `Queued` on the same frame, and
//! everything that could be slow (resolving paths, probing the destination,
//! walking the tree) happens on a worker with the card already on screen.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::path::PathPolicy;
use crate::backend::protocol::BackendStream;
use crate::backend::runtime::BackendRuntime;
use crate::backend::services::transfer::conflict::{ConflictPolicy, Decision};
use crate::backend::services::transfer::job::{
    JobId, Priority, TransferKind, TransferRequest, TransferState,
};
use crate::backend::services::transfer::pipeline::{Pipeline, PostTransferSink};
use crate::backend::services::transfer::scheduler::{Scheduler, TransferEvent};
use crate::storage::provider::StorageProvider;

/// The transfer subsystem's public face. Cheap to clone; there is one.
#[derive(Clone)]
pub struct TransferService {
    scheduler: Scheduler,
}

impl TransferService {
    /// Start the subsystem against the local filesystem.
    ///
    /// `sinks` is the post-transfer work — indexing, persistence, thumbnails —
    /// and is empty at this stage, which makes the whole seam a null check. See
    /// [`Pipeline`].
    pub fn new(
        rt: &'static BackendRuntime,
        policy: PathPolicy,
        sinks: Vec<Arc<dyn PostTransferSink>>,
    ) -> Self {
        Self::with_provider(rt, policy, crate::storage::local_dyn(), sinks)
    }

    /// Start against an explicit provider. The seam a cloud backend arrives
    /// through, and the one the engine's own tests use.
    pub fn with_provider(
        rt: &'static BackendRuntime,
        policy: PathPolicy,
        provider: Arc<dyn StorageProvider>,
        sinks: Vec<Arc<dyn PostTransferSink>>,
    ) -> Self {
        Self {
            scheduler: Scheduler::new(rt, policy, provider, Pipeline::new(rt, sinks)),
        }
    }

    // --- submission -------------------------------------------------------

    /// Copy `sources` into the directory `destination`.
    pub fn copy(
        &self,
        sources: Vec<PathBuf>,
        destination: &Path,
        policy: ConflictPolicy,
    ) -> Option<JobId> {
        self.submit(TransferRequest::copy(
            sources,
            destination.to_path_buf(),
            policy,
        ))
    }

    /// Move `sources` into the directory `destination`.
    pub fn move_to(
        &self,
        sources: Vec<PathBuf>,
        destination: &Path,
        policy: ConflictPolicy,
    ) -> Option<JobId> {
        self.submit(TransferRequest::move_to(
            sources,
            destination.to_path_buf(),
            policy,
        ))
    }

    /// Destroy `sources`, irreversibly.
    ///
    /// Its own method, and named for what it does, because the reversible
    /// operation — [`delete_to_trash`](StorageProvider::delete_to_trash) — is the
    /// default everywhere else in the app. Nothing routes here by falling back
    /// from a trash failure; a caller has to ask for this specifically, and the
    /// UI asks the user first.
    pub fn delete_permanent(&self, sources: Vec<PathBuf>) -> Option<JobId> {
        self.submit(TransferRequest::delete_permanent(sources))
    }

    /// Queue a request, or refuse it.
    ///
    /// `None` means the request could not describe an operation at all. It is not
    /// "the transfer failed" — a rejected request never becomes a job, is never
    /// announced, and is never audited, because nothing happened.
    pub fn submit(&self, mut request: TransferRequest) -> Option<JobId> {
        request.sources = dedupe(request.sources);

        if request.sources.is_empty() {
            tracing::warn!(
                target: "piku::transfer",
                kind = ?request.kind,
                "refused: no sources"
            );
            return None;
        }

        // A copy or move with no destination has nowhere to go; a permanent
        // delete with one has somewhere it is not going to use, which means the
        // caller has confused two operations.
        let wants_destination = request.kind != TransferKind::DeletePermanent;
        if wants_destination != request.destination.is_some() {
            tracing::warn!(
                target: "piku::transfer",
                kind = ?request.kind,
                has_destination = request.destination.is_some(),
                "refused: destination does not match the kind"
            );
            return None;
        }

        Some(self.scheduler.submit(request))
    }

    // --- control ----------------------------------------------------------

    /// The event stream. One consumer — see [`Scheduler::subscribe`].
    pub fn events(&self) -> BackendStream<TransferEvent> {
        self.scheduler.subscribe()
    }

    pub fn pause(&self, id: JobId) -> bool {
        self.scheduler.pause(id)
    }

    pub fn resume(&self, id: JobId) -> bool {
        self.scheduler.resume(id)
    }

    /// Returns how many jobs the request actually changed, which is what the
    /// status line reports. Jobs already stopped are not counted twice.
    pub fn pause_all(&self) -> usize {
        self.scheduler.pause_all()
    }

    pub fn resume_all(&self) -> usize {
        self.scheduler.resume_all()
    }

    pub fn cancel(&self, id: JobId) -> bool {
        self.scheduler.cancel(id)
    }

    /// Answer a job stopped in
    /// [`WaitingForInput`](TransferState::WaitingForInput).
    pub fn resolve(&self, id: JobId, decision: Decision) -> bool {
        self.scheduler.resolve(id, decision)
    }

    pub fn prioritize(&self, id: JobId, priority: Priority) -> bool {
        self.scheduler.prioritize(id, priority)
    }

    /// Live jobs in submission order. For a consumer that attached late; the
    /// store, not this, is the history.
    pub fn states(&self) -> Vec<(JobId, TransferState)> {
        self.scheduler.states()
    }

    /// Cancel every live job. Called from `Backend::shutdown` *before* the
    /// runtime drain, so the workers have their whole grace period to notice.
    pub fn shutdown(&self) {
        self.scheduler.shutdown();
    }
}

/// Drop exact repeats, keeping the first of each.
///
/// A selection that names the same path twice is a UI slip, not an instruction to
/// copy it twice — the second pass would collide with the first and produce
/// either a prompt or a `name (2)`, neither of which anyone asked for. Lexical
/// and exact, because two spellings of one file are a question about the
/// filesystem, and that question belongs to
/// [`precheck::approve`](super::precheck::approve).
///
/// `Vec::contains` rather than a set: this list is a selection, so it is tens of
/// entries, and preserving the user's order matters more than the asymptotics.
///
/// Public because the store calls it before building a card, so
/// `Copying 3 items` cannot name a count the engine will not honour.
/// [`submit`](TransferService::submit) runs it again regardless, since it cannot
/// assume its caller did.
pub fn dedupe(sources: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::with_capacity(sources.len());
    for source in sources {
        if !unique.contains(&source) {
            unique.push(source);
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backend::runtime;

    fn service() -> Option<TransferService> {
        let rt = runtime::get()?;
        Some(TransferService::new(
            rt,
            PathPolicy::with_system_roots(),
            Vec::new(),
        ))
    }

    /// The gate is structural, so it can be tested without a filesystem at all.
    #[test]
    fn a_request_that_describes_nothing_never_becomes_a_job() {
        let Some(service) = service() else {
            return;
        };

        assert!(
            service
                .copy(Vec::new(), Path::new("/tmp"), ConflictPolicy::Ask)
                .is_none(),
            "a copy with no sources was queued"
        );
        assert!(
            service.delete_permanent(Vec::new()).is_none(),
            "a delete with no sources was queued"
        );

        // A copy with no destination, built by hand — the shape a caller reaching
        // past the named verbs could produce.
        let mut headless = TransferRequest::copy(
            vec![PathBuf::from("/tmp/a")],
            PathBuf::from("/tmp/b"),
            ConflictPolicy::Ask,
        );
        headless.destination = None;
        assert!(service.submit(headless).is_none(), "a copy with no target");

        // And a delete that carries one.
        let mut misdirected = TransferRequest::delete_permanent(vec![PathBuf::from("/tmp/a")]);
        misdirected.destination = Some(PathBuf::from("/tmp/b"));
        assert!(
            service.submit(misdirected).is_none(),
            "a delete with a destination"
        );

        assert!(
            service.states().is_empty(),
            "a refused request left a job behind: {:?}",
            service
                .states()
                .iter()
                .map(|(id, state)| (id.0, state.label()))
                .collect::<Vec<_>>()
        );
    }

    /// Nothing to do, but nothing to refuse either: the job is created, and the
    /// worker is where it discovers the path is not usable.
    #[test]
    fn a_well_formed_request_is_queued_without_touching_the_disk() {
        let Some(service) = service() else {
            return;
        };

        let id = service
            .copy(
                vec![PathBuf::from("/nonexistent-source-for-a-test")],
                Path::new("/nonexistent-destination-for-a-test"),
                ConflictPolicy::Ask,
            )
            .expect("a structurally valid copy was refused");

        // Whatever it settles into, it exists and it answers.
        assert!(service.cancel(id) || service.states().is_empty());
        service.shutdown();
    }

    #[test]
    fn the_same_path_twice_is_copied_once() {
        let repeated = PathBuf::from("/tmp/a");
        let other = PathBuf::from("/tmp/b");
        assert_eq!(
            dedupe(vec![
                repeated.clone(),
                other.clone(),
                repeated.clone(),
                other.clone()
            ]),
            vec![repeated, other],
            "order changed or a repeat survived"
        );
        assert!(dedupe(Vec::new()).is_empty());
    }
}
