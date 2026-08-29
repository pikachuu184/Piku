//! The gpui-side job store: one list of [`Job`]s, and the two ways they run.
//!
//! # What this is, and what it is not
//!
//! It is not the transfer engine any more. Copy, move, and permanent delete are
//! submitted to [`TransferService`](crate::backend::services::transfer::TransferService)
//! and reported back over one long-lived event stream; the byte-moving loop, the
//! conflict resolver, the prechecks, and the throughput ring all live under
//! [`backend::services::transfer`](crate::backend::services::transfer), where no
//! gpui type can reach them. `copy_work`, `scan`, `copy_recursive`,
//! `remove_recursive`, `same_volume`, and `unique_destination` were deleted from
//! this file rather than moved, because the engine already has each of them —
//! including `remove_recursive`'s note that its containment check is mitigation
//! rather than proof, which travelled verbatim to
//! [`transfer::copy`](crate::backend::services::transfer::copy).
//!
//! What is left here is the store: the list the status bar and the transfer
//! center read, the toasts, and the simple jobs — rename, new folder, new file,
//! trash delete, git fetch — which keep the original shape of one detached
//! closure and a channel of [`JobEvent`]. They have no collisions, no suspension
//! point, and no throughput to draw, so routing them through a scheduler would
//! buy nothing and would drag `GitBackend::fetch`'s signature along with it.
//!
//! # The subscription
//!
//! `TransferService::events` is one stream for every job, and taking it replaces
//! any previous one — so exactly one consumer, and this is it. It is attached
//! lazily, on the first engine job, and *before* that job is submitted: the
//! scheduler's tick fires every 200 ms whether or not anyone is listening, and a
//! subscription taken afterwards could miss the first reading.
//!
//! The [`Inflight`] is held for the store's whole life. Dropping it ends the
//! subscription but does **not** cancel anything — a transfer belongs to the
//! engine, not to the window watching it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::{Context, Window};
use gpui_component::WindowExt as _;

use crate::backend::dispatch::{BackendExt as _, Flow};
use crate::backend::protocol::{Inflight, StreamItem};
use crate::backend::services::transfer::conflict::{ConflictPolicy, Decision};
use crate::backend::services::transfer::job::{
    JobId, Priority, TransferKind, TransferState, TransferSummary, TransferUpdate,
};
use crate::backend::services::transfer::scheduler::{Sample, TransferEvent};
use crate::backend::services::transfer::service::dedupe;
use crate::security::file_name::validate_name;
use crate::services::jobs::label;
use crate::services::jobs::{Job, JobEvent, JobKind, JobStatus, next_job_id};
use crate::state::PikuState;

const KEEP_FINISHED_JOBS: usize = 20;

/// Everything an engine submission needs, gathered into one value so
/// [`submit_to_engine`](JobQueue::submit_to_engine) does not grow a parameter
/// per field and three call sites that only differ in the fields they set.
struct SubmitRequest {
    kind: JobKind,
    /// Already-sanitized by [`label::transfer_title`] or its delete counterpart.
    title: String,
    sources: Vec<PathBuf>,
    /// `None` for the one kind that has no destination.
    destination: Option<PathBuf>,
    policy: ConflictPolicy,
}

pub struct JobQueue {
    jobs: Vec<Job>,
    /// The engine's event subscription. `None` until the first engine job; see
    /// the module header for why it is not taken at construction.
    events: Option<Inflight>,
}

impl JobQueue {
    pub fn new() -> Self {
        Self {
            jobs: Vec::new(),
            events: None,
        }
    }

    // --- reading ----------------------------------------------------------

    /// Every job, oldest first, finished ones included. The transfer center
    /// groups them itself through [`JobStatus::group`].
    pub fn jobs(&self) -> &[Job] {
        &self.jobs
    }

    /// The newest job that has not finished — what the status bar names.
    pub fn active_job(&self) -> Option<&Job> {
        self.jobs.iter().rev().find(|job| job.is_active())
    }

    pub fn active_count(&self) -> usize {
        self.jobs.iter().filter(|job| job.is_active()).count()
    }

    /// Whether anything can be paused or resumed right now, for the transfer
    /// center's two bottom-row buttons.
    pub fn can_pause_any(&self) -> bool {
        self.jobs.iter().any(Job::can_pause)
    }

    pub fn can_resume_any(&self) -> bool {
        self.jobs.iter().any(Job::can_resume)
    }

    fn find(&mut self, id: u64) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|job| job.id == id)
    }

    // --- engine jobs ------------------------------------------------------

    /// Copy or move `sources` into `dest_dir`, returning the new job's id.
    ///
    /// `policy` is the caller's standing answer for collisions.
    /// [`ConflictPolicy::Ask`] is the default and the only one that stops the job
    /// for a decision. `None` means the request described no operation and no
    /// card was created — see `TransferService::submit`.
    pub fn submit_copy(
        &mut self,
        sources: Vec<PathBuf>,
        dest_dir: PathBuf,
        is_move: bool,
        policy: ConflictPolicy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        // Deduped here as well as in the service, so the card cannot promise
        // "Copying 3 items" for a request the engine will collapse to two.
        let sources = dedupe(sources);
        let first = sources.first()?;
        let kind = if is_move {
            JobKind::Move
        } else {
            JobKind::Copy
        };
        let title = label::transfer_title(kind.label(), first, sources.len());
        self.submit_to_engine(
            SubmitRequest {
                kind,
                title,
                sources,
                destination: Some(dest_dir),
                policy,
            },
            window,
            cx,
        )
    }

    /// Destroy `paths`, irreversibly.
    ///
    /// The trash is [`submit_delete`](Self::submit_delete), and it is what every
    /// ordinary delete uses. Nothing falls back to this one: a caller has to name
    /// it, and `dialogs::confirm_delete_permanent` is the only one that does.
    pub fn submit_delete_permanent(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let paths = dedupe(paths);
        let first = paths.first()?;
        let title = label::delete_permanent_title(first, paths.len());
        self.submit_to_engine(
            SubmitRequest {
                kind: JobKind::DeletePermanent,
                title,
                sources: paths,
                destination: None,
                policy: ConflictPolicy::Ask,
            },
            window,
            cx,
        )
    }

    fn submit_to_engine(
        &mut self,
        request: SubmitRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        // Before the submit, not after: the tick does not wait for a listener.
        self.attach(window, cx);

        // Cloned out of the context rather than borrowed, so the borrow ends
        // before the `cx.notify()` below.
        let transfer = PikuState::global(cx).backend().transfer().clone();
        let id = match (request.kind, request.destination.as_deref()) {
            (JobKind::Copy, Some(dest)) => {
                transfer.copy(request.sources.clone(), dest, request.policy.clone())
            }
            (JobKind::Move, Some(dest)) => {
                transfer.move_to(request.sources.clone(), dest, request.policy.clone())
            }
            (JobKind::DeletePermanent, None) => transfer.delete_permanent(request.sources.clone()),
            // Unreachable through the two public verbs above, and refused by the
            // service anyway. Nothing is submitted and no card appears, which is
            // the same answer the service would give.
            _ => {
                tracing::warn!(
                    target: "piku::transfer",
                    kind = request.kind.label(),
                    "refused: destination does not match the kind"
                );
                None
            }
        }?;

        // The engine's id *is* the card's id — see `job::next_job_id`.
        self.jobs.push(Job::queued(
            id.0,
            request.kind,
            request.title,
            request.sources,
            request.destination,
            request.policy,
        ));
        self.trim();
        cx.notify();
        Some(id.0)
    }

    /// Take the engine's event stream, once.
    fn attach(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.events.is_some() {
            return;
        }
        self.events = Some(cx.backend_stream_in(
            window,
            |backend| backend.transfer().events(),
            |queue, item, window, cx| queue.apply_stream_item(item, window, cx),
        ));
    }

    fn apply_stream_item(
        &mut self,
        item: StreamItem<TransferEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Flow {
        match item {
            StreamItem::Batch(events) => {
                for event in events {
                    self.apply_transfer_event(event, window, cx);
                }
            }
            // Per-job readings ride on `TransferEvent::Tick`, which carries one
            // entry per job. The protocol's single-figure `Progress` cannot say
            // which job it belongs to, so the engine never sends it.
            StreamItem::Progress(_) => {}
            StreamItem::Done(result) => {
                if let Err(error) = &result
                    && !error.is_cancelled()
                {
                    tracing::warn!(target: "piku::transfer", %error, "event stream ended");
                }
                // Drop the leash so the next engine job re-subscribes rather
                // than running with nothing watching it.
                self.events = None;
            }
        }
        Flow::Continue
    }

    fn apply_transfer_event(
        &mut self,
        event: TransferEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TransferEvent::Job { id, update } => match update {
                TransferUpdate::State(state) => self.apply_state(id, state),
                TransferUpdate::Failed(failure) => {
                    if let Some(job) = self.find(id.0) {
                        job.failures.push(failure);
                    }
                }
            },
            TransferEvent::Tick(samples) => {
                for sample in samples {
                    self.apply_sample(sample);
                }
            }
            // The last thing said about an id, and it arrives in the same batch
            // as the terminal state above — so by here `job.status` is already
            // terminal and this only has to say so out loud.
            TransferEvent::Finished { id, summary } => self.announce(id, summary, window, cx),
        }
    }

    fn apply_state(&mut self, id: JobId, state: TransferState) {
        let Some(job) = self.find(id.0) else {
            return;
        };
        // Cleared on every transition: a job that left `WaitingForInput` has no
        // unanswered conflicts, and a stale list would put a resolved dialog back
        // on screen.
        job.pending.clear();
        job.status = match state {
            TransferState::Queued => JobStatus::Queued,
            TransferState::Running => JobStatus::Running,
            TransferState::Pausing => JobStatus::Pausing,
            TransferState::Paused => JobStatus::Paused,
            TransferState::WaitingForInput { pending } => {
                job.pending = pending;
                JobStatus::WaitingForInput
            }
            TransferState::Completed => JobStatus::Done,
            TransferState::Failed(error) => JobStatus::Failed(label::transfer_error(&error)),
            TransferState::Cancelled => JobStatus::Cancelled,
        };
    }

    fn apply_sample(&mut self, sample: Sample) {
        let Some(job) = self.find(sample.id.0) else {
            return;
        };
        job.total_bytes = sample.progress.total_bytes;
        job.done_bytes = sample.progress.done_bytes;
        job.total_items = sample.progress.total_items as usize;
        job.done_items = sample.progress.done_items as usize;
        job.rate = sample.waveform;
        job.throughput = sample.bytes_per_sec;
        job.eta = sample.eta;
        job.current_item = sample.current_item;
    }

    /// The one toast per engine job, raised from the summary so the count it
    /// reads is the count the engine actually moved.
    fn announce(
        &mut self,
        id: JobId,
        summary: TransferSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.find(id.0) else {
            return;
        };
        // Nothing is moving any more, whatever the last tick said.
        job.throughput = 0;
        job.eta = None;
        job.current_item = None;
        // Only the summary carries this, and it is true of every outcome: a
        // cancelled copy skipped whatever it had been told to skip before it
        // stopped.
        job.skipped = summary.skipped;

        let items = summary.items as usize;
        match &job.status {
            JobStatus::Done => {
                // The authoritative counts. A dropped tick can leave the bar
                // short of its total, and a card reading 812 / 1,032 next to
                // "Completed" is the kind of thing a user reports as data loss.
                job.done_items = job.total_items.max(items);
                job.done_bytes = job.total_bytes.max(summary.bytes);
                let message = match summary.kind {
                    TransferKind::Copy => label::transfer_summary(false, items),
                    TransferKind::Move => label::transfer_summary(true, items),
                    TransferKind::DeletePermanent => label::delete_permanent_summary(items),
                };
                window.push_notification(crate::ui::toast::success(message), cx);
            }
            JobStatus::Failed(message) => {
                let message = message.clone();
                window.push_notification(crate::ui::toast::error(message), cx);
            }
            JobStatus::Cancelled => {
                window.push_notification(crate::ui::toast::info(label::CANCELLED_TOAST), cx);
            }
            // Terminal counts for a job the store does not think has finished.
            // Nothing to announce, and inventing an outcome would be worse.
            live => tracing::warn!(
                target: "piku::transfer",
                job = %id,
                status = label::status_label(live),
                "summary arrived for a live job"
            ),
        }
        cx.notify();
    }

    // --- controls ---------------------------------------------------------
    //
    // Engine jobs route through the service; a simple job has only its flag.
    // `JobKind::uses_engine` is what decides, so no call site has to.

    pub fn pause(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.find(id).is_some_and(|job| job.can_pause()) {
            PikuState::global(cx).backend().transfer().pause(JobId(id));
            cx.notify();
        }
    }

    pub fn resume(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.find(id).is_some_and(|job| job.can_resume()) {
            PikuState::global(cx).backend().transfer().resume(JobId(id));
            cx.notify();
        }
    }

    /// Pause every engine job. Simple jobs are untouched — there is nothing in a
    /// rename to pause.
    pub fn pause_all(&mut self, cx: &mut Context<Self>) {
        PikuState::global(cx).backend().transfer().pause_all();
        cx.notify();
    }

    pub fn resume_all(&mut self, cx: &mut Context<Self>) {
        PikuState::global(cx).backend().transfer().resume_all();
        cx.notify();
    }

    /// Stop one job. Engine jobs go through the service, which releases the
    /// permit and writes the audit record; a flag could do neither.
    pub fn cancel(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.find(id) else {
            return;
        };
        if !job.can_cancel() {
            return;
        }
        if job.kind.uses_engine() {
            PikuState::global(cx).backend().transfer().cancel(JobId(id));
        } else {
            job.cancel.store(true, Ordering::Relaxed);
        }
        cx.notify();
    }

    /// Answer a job stopped in [`JobStatus::WaitingForInput`].
    ///
    /// The status is not written here. The engine decides what an answer means — a
    /// [`Decision`] that does not settle every clash leaves the job waiting — and
    /// it says so on the next state event.
    pub fn resolve(&mut self, id: u64, decision: Decision, cx: &mut Context<Self>) {
        PikuState::global(cx)
            .backend()
            .transfer()
            .resolve(JobId(id), decision);
        cx.notify();
    }

    pub fn prioritize(&mut self, id: u64, priority: Priority, cx: &mut Context<Self>) {
        PikuState::global(cx)
            .backend()
            .transfer()
            .prioritize(JobId(id), priority);
        cx.notify();
    }

    /// Re-submit a failed or cancelled job as a new one.
    ///
    /// A new card with a new id rather than a reset of the old one: the old card
    /// is the record of what happened, and overwriting it would take away the
    /// failure rows the user is reading while they decide to retry.
    pub fn retry(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) -> Option<u64> {
        let job = self.find(id).filter(|job| job.can_retry())?;
        let kind = job.kind;
        let sources = job.sources.clone();
        let destination = job.destination.clone();
        let policy = job.policy.clone();
        let retries = job.retries + 1;

        // `can_retry` is exhaustive over `JobKind` and has already refused the
        // kinds whose request does not survive on the card.
        let fresh = match kind {
            JobKind::Copy | JobKind::Move => self.submit_copy(
                sources,
                destination?,
                kind == JobKind::Move,
                policy,
                window,
                cx,
            )?,
            JobKind::DeletePermanent => self.submit_delete_permanent(sources, window, cx)?,
            JobKind::Delete => self.submit_delete(sources, window, cx)?,
            JobKind::Rename | JobKind::NewFolder | JobKind::NewFile | JobKind::GitFetch => {
                return None;
            }
        };
        if let Some(job) = self.find(fresh) {
            job.retries = retries;
        }
        Some(fresh)
    }

    /// Drop one finished job from the list. A live job is kept: the button for
    /// those is Cancel.
    pub fn remove(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.jobs.len();
        self.jobs.retain(|job| job.id != id || job.is_active());
        if self.jobs.len() != before {
            cx.notify();
        }
    }

    /// Drop every finished job.
    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        let before = self.jobs.len();
        self.jobs.retain(Job::is_active);
        if self.jobs.len() != before {
            cx.notify();
        }
    }

    // --- simple jobs ------------------------------------------------------

    /// Move to the trash. Reversible, and what every ordinary delete uses.
    pub fn submit_delete(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<u64> {
        let first = paths.first()?;
        let title = label::delete_title(first, paths.len());
        let provider = crate::storage::local_dyn();
        let listed = paths.clone();
        Some(self.spawn_job(
            JobKind::Delete,
            title,
            listed,
            window,
            cx,
            move |tx, _cancel| {
                let count = paths.len();
                let _ = tx.unbounded_send(JobEvent::Scanned {
                    total_bytes: 0,
                    total_items: count,
                });
                let result = provider
                    .delete_to_trash(&paths)
                    .map(|_| label::delete_summary(count))
                    .map_err(|error| error.to_string());
                let _ = tx.unbounded_send(JobEvent::Progress {
                    delta_bytes: 0,
                    delta_items: count,
                });
                let _ = tx.unbounded_send(JobEvent::Finished(result));
            },
        ))
    }

    pub fn submit_rename(
        &mut self,
        from: PathBuf,
        to: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Defense in depth: dialogs validate too, but every rename that
        // reaches the engine gets checked regardless of caller.
        if let Some(name) = to.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::rename_title(&from);
        let provider = crate::storage::local_dyn();
        let listed = vec![from.clone()];
        self.spawn_job(
            JobKind::Rename,
            title,
            listed,
            window,
            cx,
            move |tx, _cancel| {
                let result = provider
                    .rename(&from, &to)
                    .map(|_| label::rename_summary(&to))
                    .map_err(|error| error.to_string());
                let _ = tx.unbounded_send(JobEvent::Finished(result));
            },
        );
    }

    pub fn submit_new_folder(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::create_title(&path);
        let provider = crate::storage::local_dyn();
        let listed = vec![path.clone()];
        self.spawn_job(
            JobKind::NewFolder,
            title,
            listed,
            window,
            cx,
            move |tx, _cancel| {
                let result = provider
                    .create_dir(&path)
                    .map(|_| label::create_summary(&path))
                    .map_err(|error| error.to_string());
                let _ = tx.unbounded_send(JobEvent::Finished(result));
            },
        );
    }

    pub fn submit_new_file(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned())
            && let Err(error) = validate_name(&name)
        {
            window.push_notification(crate::ui::toast::error(error), cx);
            return;
        }
        let title = label::create_title(&path);
        let provider = crate::storage::local_dyn();
        let listed = vec![path.clone()];
        self.spawn_job(
            JobKind::NewFile,
            title,
            listed,
            window,
            cx,
            move |tx, _cancel| {
                let result = provider
                    .create_file(&path)
                    .map(|_| label::create_summary(&path))
                    .map_err(|error| error.to_string());
                let _ = tx.unbounded_send(JobEvent::Finished(result));
            },
        );
    }

    /// Fetch one remote of a repository as a background job (progress and
    /// cancel share the transfer center's UI). Completion writes an audit
    /// record and asks the `GitStore` to refresh the repository.
    pub fn submit_git_fetch(
        &mut self,
        root: PathBuf,
        remote: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = label::fetch_title(&remote);
        let backend = PikuState::global(cx).git.read(cx).backend();
        let job_root = root;
        let listed = vec![job_root.clone()];
        self.spawn_job(
            JobKind::GitFetch,
            title,
            listed,
            window,
            cx,
            move |tx, cancel| {
                use crate::services::git::backend::GitBackend as _;
                let result = backend.fetch(&job_root, &remote, &tx, &cancel);
                let (ok, message) = match &result {
                    Ok(outcome) => (
                        true,
                        label::fetch_summary(&outcome.remote, outcome.updated_refs),
                    ),
                    Err(error) => (false, error.to_string()),
                };
                crate::security::audit::record("git.fetch", &job_root, None, ok, &remote);
                let _ = tx.unbounded_send(JobEvent::Finished(if ok {
                    Ok(message)
                } else if cancel.load(Ordering::Relaxed) {
                    Err(label::CANCELLED.into())
                } else {
                    Err(message)
                }));
            },
        );
        // No explicit refresh needed: a fetch that updated anything touched
        // `.git`/`.git/refs/remotes`, which the repo watchers already cover.
    }

    /// Push a card and run `work` on the background executor, returning the new
    /// job's id.
    ///
    /// `listed` is what the job acts on. It is carried on the card for the
    /// identity row and for "Show in folder"; a simple job's retry, where it is
    /// offered at all, rebuilds itself from the same field.
    fn spawn_job(
        &mut self,
        kind: JobKind,
        title: String,
        listed: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(UnboundedSender<JobEvent>, Arc<AtomicBool>) + Send + 'static,
    ) -> u64 {
        // From the engine's counter, so a rename and a copy can never share an
        // id — see `job::next_job_id`.
        let id = next_job_id();
        let mut job = Job::new(id, kind, title);
        job.sources = listed;
        let cancel = job.cancel.clone();
        self.jobs.push(job);
        self.trim();
        cx.notify();

        let (tx, mut rx) = unbounded::<JobEvent>();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .spawn(async move {
                    work(tx, cancel);
                })
                .detach();

            while let Some(event) = rx.next().await {
                let finished = matches!(event, JobEvent::Finished(_));
                if this
                    .update_in(cx, |queue, window, cx| {
                        queue.apply_event(id, event, window, cx);
                    })
                    .is_err()
                    || finished
                {
                    break;
                }
            }
        })
        .detach();

        id
    }

    fn apply_event(
        &mut self,
        id: u64,
        event: JobEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.find(id) else {
            return;
        };
        match event {
            JobEvent::Scanned {
                total_bytes,
                total_items,
            } => {
                job.total_bytes = total_bytes;
                job.total_items = total_items;
            }
            JobEvent::Progress {
                delta_bytes,
                delta_items,
            } => {
                job.done_bytes += delta_bytes;
                job.done_items += delta_items;
            }
            JobEvent::Finished(result) => match result {
                Ok(message) => {
                    job.status = JobStatus::Done;
                    window.push_notification(crate::ui::toast::success(message), cx);
                }
                Err(error) => {
                    if label::is_cancelled(&error) {
                        job.status = JobStatus::Cancelled;
                        window
                            .push_notification(crate::ui::toast::info(label::CANCELLED_TOAST), cx);
                    } else {
                        job.status = JobStatus::Failed(error.clone());
                        window.push_notification(crate::ui::toast::error(error), cx);
                    }
                }
            },
        }
        cx.notify();
    }

    /// Keep the history bounded. The oldest finished jobs go first; a live job is
    /// never dropped, however long the list gets.
    fn trim(&mut self) {
        let finished = self.jobs.iter().filter(|job| !job.is_active()).count();
        if finished > KEEP_FINISHED_JOBS {
            let mut to_remove = finished - KEEP_FINISHED_JOBS;
            self.jobs.retain(|job| {
                if to_remove > 0 && !job.is_active() {
                    to_remove -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }
}
