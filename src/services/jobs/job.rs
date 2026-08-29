//! Background job model: every mutating operation is a job with its own
//! progress, lifecycle, and way to be stopped.
//!
//! # One model, two engines
//!
//! Copy, move, and permanent delete run on the transfer engine in
//! [`backend::services::transfer`](crate::backend::services::transfer) — bounded
//! concurrency, pause, conflict resolution, throughput sampling. Rename, new
//! folder, new file, trash delete, and git fetch keep the simpler path: one
//! detached closure, a channel of [`JobEvent`], done.
//!
//! Both fill in the same [`Job`], and the transfer center draws one list. A
//! simple job leaves [`Job::rate`] empty and gets the completion ring alone,
//! which is honest — there is no throughput to draw for a rename.
//! [`JobKind::uses_engine`] is what decides which half a job belongs to, so the
//! routing lives on the kind rather than in every call site.
//!
//! # Why the ids come from `JobId::next`
//!
//! The store used to allocate its own `next_id` from 1, and the engine allocates
//! [`JobId`] from 1, so the two would have collided the moment a copy and a
//! rename shared a session. Both now draw from the engine's counter, which makes
//! [`Job::id`] the engine's id for engine jobs and a unique-anyway id for the
//! rest. Nothing has to translate.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::backend::services::transfer::conflict::{Conflict, ConflictPolicy};
use crate::backend::services::transfer::job::{JobId, StateGroup, TransferFailure};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    /// To the trash, and reversible.
    Delete,
    /// Irreversible. A separate kind from [`Delete`](Self::Delete) rather than a
    /// flag on it, so no code path can reach it by accident.
    DeletePermanent,
    Rename,
    NewFolder,
    NewFile,
    GitFetch,
}

impl JobKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Delete => "Deleting",
            Self::DeletePermanent => "Deleting permanently",
            Self::Rename => "Renaming",
            Self::NewFolder => "Creating folder",
            Self::NewFile => "Creating file",
            Self::GitFetch => "Fetching",
        }
    }

    /// Whether this kind runs on the transfer engine, and therefore whether
    /// pause, resume, cancel, and conflict answers route through
    /// [`TransferService`](crate::backend::services::transfer::TransferService)
    /// rather than the job's own cancel flag.
    pub fn uses_engine(&self) -> bool {
        matches!(self, Self::Copy | Self::Move | Self::DeletePermanent)
    }

    /// Whether re-running this needs the same confirmation the first run needed.
    ///
    /// Only permanent delete. Every other kind is either recoverable (the trash, a
    /// rename) or already-asked-for and harmless to re-run (a copy, and a move
    /// whose remaining items are still where they were). Permanent deletion is the
    /// one where the kebab's `Retry` would otherwise be a quieter route to exactly
    /// what [`confirm_delete_permanent`](crate::ui::explorer::dialogs) exists to
    /// slow down.
    pub fn needs_reconfirm(&self) -> bool {
        matches!(self, Self::DeletePermanent)
    }
}

/// Where a job is in its life.
///
/// The four original variants — `Running`, `Done`, `Failed`, `Cancelled` — are
/// unchanged, and the four new ones are the engine's states arriving. A second
/// `state` field beside this one was the other option and is worse: two fields
/// that must agree are two fields that will not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobStatus {
    /// Admitted, waiting for a permit. Engine jobs only.
    Queued,
    Running,
    /// Pause requested, current chunk draining. Engine jobs only.
    Pausing,
    Paused,
    /// Stopped for a decision, and cannot leave without one. The conflicts are
    /// on [`Job::pending`].
    WaitingForInput,
    Done,
    Failed(String),
    Cancelled,
}

impl JobStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done | Self::Failed(_) | Self::Cancelled)
    }

    /// Which section of the transfer center this job is listed under.
    ///
    /// Reuses the engine's own grouping rather than restating it, so the sections
    /// cannot drift apart from the states that feed them.
    pub fn group(&self) -> StateGroup {
        match self {
            Self::Queued => StateGroup::Queued,
            Self::Running | Self::Pausing | Self::Paused => StateGroup::Active,
            Self::WaitingForInput => StateGroup::WaitingForInput,
            Self::Done | Self::Failed(_) | Self::Cancelled => StateGroup::Completed,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    /// From [`JobId::next`], for every job whichever engine runs it. For an
    /// engine job this *is* the engine's id, which is what the store passes back
    /// to `pause`, `cancel`, and `resolve`.
    pub id: u64,
    pub kind: JobKind,
    /// Already-sanitized display title. Never a raw path.
    pub title: String,
    /// What is being moved and where. Kept for the card's identity row, for
    /// "Show in folder", and for a retry — a retry needs to re-submit the same
    /// request, and the request is these two fields plus the policy.
    pub sources: Vec<PathBuf>,
    pub destination: Option<PathBuf>,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: usize,
    pub done_items: usize,
    pub status: JobStatus,
    /// Throughput history, newest last, normalized to `0.0..=1.0`. Empty for a
    /// simple job, which has nothing to sample.
    pub rate: Vec<f32>,
    /// Bytes per second, as of the last tick.
    pub throughput: u64,
    /// `None` until there are enough samples to mean anything.
    pub eta: Option<Duration>,
    /// The item in flight, sanitized by the engine before it ever gets here.
    pub current_item: Option<Arc<str>>,
    /// What to do about collisions, carried so a retry repeats the answer the
    /// user already gave.
    pub policy: ConflictPolicy,
    /// Unanswered conflicts, non-empty exactly when the status is
    /// [`WaitingForInput`](JobStatus::WaitingForInput).
    pub pending: Vec<Conflict>,
    /// How many times this job has been re-submitted from the kebab menu.
    pub retries: u32,
    /// Items the job skipped over. A failure here did not fail the job — a copy
    /// of 40 000 files is not abandoned because one of them is locked — so these
    /// are listed on the expanded card rather than raised as the job's status.
    pub failures: Vec<TransferFailure>,
    /// Items stepped over *by policy* — a `Skip` answer to a collision — which is
    /// a different thing from [`failures`](Self::failures) and counted separately
    /// by the engine. Only meaningful once the terminal summary has arrived, since
    /// that is what carries it.
    pub skipped: u64,
    /// The stop flag for a simple job. Engine jobs ignore it and route
    /// cancellation through the service, because a flag cannot release a
    /// scheduler permit or write an audit record.
    pub cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn new(id: u64, kind: JobKind, title: String) -> Self {
        Self {
            id,
            kind,
            title,
            sources: Vec::new(),
            destination: None,
            total_bytes: 0,
            done_bytes: 0,
            total_items: 0,
            done_items: 0,
            status: JobStatus::Running,
            rate: Vec::new(),
            throughput: 0,
            eta: None,
            current_item: None,
            policy: ConflictPolicy::Ask,
            pending: Vec::new(),
            retries: 0,
            failures: Vec::new(),
            skipped: 0,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An engine job, which starts [`Queued`](JobStatus::Queued) rather than
    /// `Running`: it has not been given a permit yet, and saying otherwise is how
    /// a status bar comes to claim a copy is running while four others hold every
    /// permit.
    pub fn queued(
        id: u64,
        kind: JobKind,
        title: String,
        sources: Vec<PathBuf>,
        destination: Option<PathBuf>,
        policy: ConflictPolicy,
    ) -> Self {
        Self {
            status: JobStatus::Queued,
            sources,
            destination,
            policy,
            ..Self::new(id, kind, title)
        }
    }

    /// Whether the job still counts toward "3 active".
    pub fn is_active(&self) -> bool {
        !self.status.is_terminal()
    }

    /// Whether bytes are moving right now — the narrower question the status bar
    /// asks, since a queued or paused job has a progress bar that is not going
    /// anywhere.
    pub fn is_moving(&self) -> bool {
        matches!(self.status, JobStatus::Running | JobStatus::Pausing)
    }

    /// Whether the pause button should do anything. Only engine jobs can pause;
    /// a rename has no suspension point to reach.
    pub fn can_pause(&self) -> bool {
        self.kind.uses_engine() && matches!(self.status, JobStatus::Queued | JobStatus::Running)
    }

    pub fn can_resume(&self) -> bool {
        self.kind.uses_engine() && matches!(self.status, JobStatus::Pausing | JobStatus::Paused)
    }

    pub fn can_cancel(&self) -> bool {
        !self.status.is_terminal()
    }

    /// Whether the kebab menu's "Retry" applies: it failed or was cancelled, and
    /// there is enough left on the job to rebuild the request.
    ///
    /// Exhaustive over [`JobKind`] rather than keyed off
    /// [`uses_engine`](JobKind::uses_engine), because the question is not which
    /// engine ran it — it is whether the card still holds the whole request. A new
    /// kind has to answer for itself here, which is the point.
    pub fn can_retry(&self) -> bool {
        matches!(self.status, JobStatus::Failed(_) | JobStatus::Cancelled)
            && !self.sources.is_empty()
            && match self.kind {
                // Needs somewhere to go, and it has to still be on the card.
                JobKind::Copy | JobKind::Move => self.destination.is_some(),
                // The sources are the whole request.
                JobKind::Delete | JobKind::DeletePermanent => true,
                // A rename needs its new name and a create needs the name it was
                // given. Neither survives on the card, so neither is offered
                // rather than offered and then quietly declined.
                JobKind::Rename | JobKind::NewFolder | JobKind::NewFile | JobKind::GitFetch => {
                    false
                }
            }
    }

    /// Progress in `0.0..=100.0`, byte-weighted when byte totals are known.
    pub fn percent(&self) -> f32 {
        if self.total_bytes > 0 {
            (self.done_bytes as f64 / self.total_bytes as f64 * 100.0) as f32
        } else if self.total_items > 0 {
            (self.done_items as f64 / self.total_items as f64 * 100.0) as f32
        } else {
            0.0
        }
    }

    /// The job-level error, if it failed. Per-item failures are
    /// [`failures`](Self::failures) and are not this.
    pub fn error(&self) -> Option<&str> {
        match &self.status {
            JobStatus::Failed(message) => Some(message.as_str()),
            _ => None,
        }
    }
}

/// A fresh id, from the same counter the engine uses. See the module header.
pub fn next_job_id() -> u64 {
    JobId::next().0
}

/// Events streamed from a simple job's worker thread back to the UI executor.
///
/// Engine jobs do not use this: they are published by the scheduler's 200 ms
/// tick as [`TransferEvent`](crate::backend::services::transfer::scheduler::TransferEvent),
/// which carries throughput and ETA this cannot.
pub enum JobEvent {
    Scanned {
        total_bytes: u64,
        total_items: usize,
    },
    Progress {
        delta_bytes: u64,
        delta_items: usize,
    },
    Finished(Result<String, String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The routing question every control button asks. Getting it wrong means a
    /// pause button that does nothing, or a copy cancelled by a flag nobody
    /// reads.
    #[test]
    fn only_the_three_engine_kinds_use_the_engine() {
        for kind in [JobKind::Copy, JobKind::Move, JobKind::DeletePermanent] {
            assert!(kind.uses_engine(), "{} should use the engine", kind.label());
        }
        for kind in [
            JobKind::Delete,
            JobKind::Rename,
            JobKind::NewFolder,
            JobKind::NewFile,
            JobKind::GitFetch,
        ] {
            assert!(!kind.uses_engine(), "{} should not", kind.label());
        }
        // Trash and permanent delete are never the same thing.
        assert_ne!(JobKind::Delete, JobKind::DeletePermanent);
        assert!(!JobKind::Delete.uses_engine() && JobKind::DeletePermanent.uses_engine());
    }

    #[test]
    fn the_new_states_group_the_way_the_engine_groups_them() {
        let cases = [
            (JobStatus::Queued, StateGroup::Queued),
            (JobStatus::Running, StateGroup::Active),
            (JobStatus::Pausing, StateGroup::Active),
            (JobStatus::Paused, StateGroup::Active),
            (JobStatus::WaitingForInput, StateGroup::WaitingForInput),
            (JobStatus::Done, StateGroup::Completed),
            (JobStatus::Failed("x".into()), StateGroup::Completed),
            (JobStatus::Cancelled, StateGroup::Completed),
        ];
        for (status, group) in cases {
            assert_eq!(status.group(), group, "{status:?} grouped wrong");
            assert_eq!(
                status.is_terminal(),
                group == StateGroup::Completed,
                "{status:?} disagrees with its own group about being finished"
            );
        }
    }

    /// A queued copy is not a running copy, and the buttons have to know.
    #[test]
    fn a_queued_engine_job_can_pause_before_it_ever_ran() {
        let job = Job::queued(
            1,
            JobKind::Copy,
            "Copying “a”".into(),
            vec![PathBuf::from("/a")],
            Some(PathBuf::from("/b")),
            ConflictPolicy::Replace,
        );
        assert_eq!(job.status, JobStatus::Queued);
        assert!(job.is_active() && !job.is_moving());
        assert!(job.can_pause() && job.can_cancel());
        assert!(!job.can_resume(), "a queued job has nothing to resume");
        assert!(!job.can_retry(), "a live job is not a retry candidate");
        assert!(job.rate.is_empty() && job.eta.is_none());
    }

    /// A rename has no suspension point, so offering to pause it would be a lie.
    #[test]
    fn a_simple_job_offers_no_pause() {
        let mut job = Job::new(2, JobKind::Rename, "Renaming “a”".into());
        assert_eq!(job.status, JobStatus::Running);
        assert!(job.is_moving() && job.can_cancel());
        assert!(!job.can_pause() && !job.can_resume());

        job.status = JobStatus::Done;
        assert!(!job.is_active() && !job.can_cancel());
    }

    #[test]
    fn retry_needs_enough_of_the_request_left_to_rebuild_it() {
        let mut job = Job::queued(
            3,
            JobKind::Copy,
            "Copying “a”".into(),
            vec![PathBuf::from("/a")],
            Some(PathBuf::from("/b")),
            ConflictPolicy::Ask,
        );
        job.status = JobStatus::Failed("disk full".into());
        assert!(job.can_retry());
        assert_eq!(job.error(), Some("disk full"));

        job.destination = None;
        assert!(!job.can_retry(), "a copy with nowhere to go was retryable");

        job.status = JobStatus::Cancelled;
        job.sources.clear();
        assert!(!job.can_retry());
        assert_eq!(job.error(), None, "cancelled is not failed");

        // A permanent delete has no destination and never needs one.
        let mut wipe = Job::queued(
            4,
            JobKind::DeletePermanent,
            "Deleting “a” permanently".into(),
            vec![PathBuf::from("/a")],
            None,
            ConflictPolicy::Ask,
        );
        wipe.status = JobStatus::Cancelled;
        assert!(
            wipe.can_retry(),
            "a stopped permanent delete cannot restart"
        );

        // A simple job carries its paths for the card's identity row, so the
        // gate cannot be "does it have sources": a rename's new name is not
        // among them.
        let mut renamed = Job::new(5, JobKind::Rename, "Renaming “a”".into());
        renamed.sources = vec![PathBuf::from("/a")];
        renamed.status = JobStatus::Failed("permission denied".into());
        assert!(
            !renamed.can_retry(),
            "a rename cannot be rebuilt from a card"
        );
    }

    #[test]
    fn percent_prefers_bytes_and_falls_back_to_items() {
        let mut job = Job::new(4, JobKind::Copy, "t".into());
        assert_eq!(job.percent(), 0.0);

        job.total_items = 4;
        job.done_items = 1;
        assert_eq!(job.percent(), 25.0);

        // Bytes win once they are known, because a 4-file copy where one file is
        // 9 GB is not 25% done after the first small one.
        job.total_bytes = 1000;
        job.done_bytes = 900;
        assert_eq!(job.percent(), 90.0);
    }

    #[test]
    fn ids_are_unique_across_both_engines() {
        let first = next_job_id();
        let second = next_job_id();
        // Same counter the engine draws from, so a copy and a rename can never
        // collide.
        let engine = JobId::next().0;
        assert!(first < second && second < engine);
    }
}
