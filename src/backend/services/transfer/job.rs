//! Job identity, kind, priority, lifecycle, and the two payloads that cross
//! the wire: [`TransferUpdate`] going out and [`TransferRequest`] coming in.
//!
//! Nothing here touches the filesystem or the runtime, so the state machine is
//! testable on its own — which is the point of the test at the bottom.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::backend::error::{TransferError, TransferVerb};
use crate::backend::services::transfer::conflict::{Conflict, ConflictPolicy};

/// Immutable job identity, allocated once and never reused.
///
/// Process-wide rather than per-service, so a log line naming job 41 is
/// unambiguous even after the queue has been trimmed.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct JobId(pub u64);

impl JobId {
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a job does.
///
/// Only these three go through the engine. Rename, new folder, new file, trash
/// delete, and git fetch have no collisions, no suspension point, and no
/// throughput to draw, so they keep the simpler path in `services::jobs`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TransferKind {
    Copy,
    Move,
    /// Irreversible removal. Deliberately a separate operation from moving to
    /// the trash, never a fallback for it.
    DeletePermanent,
}

impl TransferKind {
    /// The direction data moves, for the two failures that need to name it.
    /// `None` for a delete, which moves nothing anywhere.
    pub fn verb(self) -> Option<TransferVerb> {
        match self {
            Self::Copy => Some(TransferVerb::Copy),
            Self::Move => Some(TransferVerb::Move),
            Self::DeletePermanent => None,
        }
    }

    /// Whether completing this job destroys the source. Copy is the only one
    /// that does not, and the only one that therefore needs no audit record per
    /// item beyond what the provider already writes.
    pub fn destroys_source(self) -> bool {
        !matches!(self, Self::Copy)
    }

    /// The audit op name for the destructive half of this kind.
    pub fn audit_op(self) -> &'static str {
        match self {
            Self::Copy => "transfer.copy",
            Self::Move => "transfer.move",
            Self::DeletePermanent => "transfer.delete_permanent",
        }
    }
}

/// Scheduling class. Ordered, and the order is the drain order.
///
/// `Interactive` is a paste the user is watching. `Normal` is everything else
/// the user asked for. `Background` is follow-up work nobody is waiting on —
/// which is where the post-transfer pipeline will land.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Priority {
    Interactive,
    #[default]
    Normal,
    Background,
}

impl Priority {
    pub const COUNT: usize = 3;

    pub fn index(self) -> usize {
        match self {
            Self::Interactive => 0,
            Self::Normal => 1,
            Self::Background => 2,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Normal => "normal",
            Self::Background => "background",
        }
    }
}

/// The job lifecycle.
///
/// `Failed` carries its error behind an `Arc` so the whole state is cheap to
/// clone out of the control cell for publishing — `TransferError` wraps
/// `io::Error`, which is neither `Clone` nor `PartialEq`.
#[derive(Clone, Debug)]
pub enum TransferState {
    /// Admitted, waiting for a permit.
    Queued,
    /// A worker holds a permit and is moving bytes.
    Running,
    /// Pause requested; the current chunk is draining. The honest intermediate
    /// — without it, a UI that flips straight to `Paused` is lying for as long
    /// as the write takes.
    Pausing,
    /// The worker reached a safe suspension point and stopped reading and
    /// writing. Resume needs no answer from anyone.
    Paused,
    /// Stopped for a decision, and cannot leave without one.
    WaitingForInput {
        pending: Vec<Conflict>,
    },
    Completed,
    Failed(Arc<TransferError>),
    Cancelled,
}

/// Which section of the transfer center a job is listed under.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum StateGroup {
    Active,
    WaitingForInput,
    Queued,
    Completed,
}

impl StateGroup {
    pub fn title(self) -> &'static str {
        match self {
            Self::Active => "Active",
            Self::WaitingForInput => "Waiting for Input",
            Self::Queued => "Queued",
            Self::Completed => "Completed",
        }
    }
}

/// How a job leaves the state it is in.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ResumePath {
    /// Nothing to resume — the worker is already moving, or waiting only for a
    /// permit.
    NotStopped,
    /// `resume()` alone restarts it.
    Automatic,
    /// A [`Decision`](super::conflict::Decision) must arrive first.
    Decision,
    /// Terminal; it does not leave.
    Terminal,
}

impl TransferState {
    pub fn group(&self) -> StateGroup {
        match self {
            Self::Queued => StateGroup::Queued,
            Self::Running | Self::Pausing | Self::Paused => StateGroup::Active,
            Self::WaitingForInput { .. } => StateGroup::WaitingForInput,
            Self::Completed | Self::Failed(_) | Self::Cancelled => StateGroup::Completed,
        }
    }

    pub fn resume_path(&self) -> ResumePath {
        match self {
            Self::Queued | Self::Running | Self::Pausing => ResumePath::NotStopped,
            Self::Paused => ResumePath::Automatic,
            Self::WaitingForInput { .. } => ResumePath::Decision,
            Self::Completed | Self::Failed(_) | Self::Cancelled => ResumePath::Terminal,
        }
    }

    pub fn is_terminal(&self) -> bool {
        self.resume_path() == ResumePath::Terminal
    }

    /// Whether the job still counts against the "3 active" header.
    pub fn is_live(&self) -> bool {
        !self.is_terminal()
    }

    /// A short, stable name for logs and for the status line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::WaitingForInput { .. } => "waiting",
            Self::Completed => "completed",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What the UI submits.
#[derive(Clone, Debug)]
pub struct TransferRequest {
    pub kind: TransferKind,
    pub sources: Vec<PathBuf>,
    /// The destination *directory*. `None` for a permanent delete, which has
    /// nowhere to put anything.
    pub destination: Option<PathBuf>,
    pub policy: ConflictPolicy,
    pub priority: Priority,
}

impl TransferRequest {
    pub fn copy(sources: Vec<PathBuf>, destination: PathBuf, policy: ConflictPolicy) -> Self {
        Self {
            kind: TransferKind::Copy,
            sources,
            destination: Some(destination),
            policy,
            priority: Priority::Interactive,
        }
    }

    pub fn move_to(sources: Vec<PathBuf>, destination: PathBuf, policy: ConflictPolicy) -> Self {
        Self {
            kind: TransferKind::Move,
            sources,
            destination: Some(destination),
            policy,
            priority: Priority::Interactive,
        }
    }

    pub fn delete_permanent(sources: Vec<PathBuf>) -> Self {
        Self {
            kind: TransferKind::DeletePermanent,
            sources,
            destination: None,
            policy: ConflictPolicy::Ask,
            priority: Priority::Interactive,
        }
    }
}

/// One item the job could not handle, kept so the expanded card can list it.
///
/// A failure here does not fail the job: a copy of 40 000 files should not be
/// abandoned because one of them is locked.
#[derive(Clone, Debug)]
pub struct TransferFailure {
    /// Already sanitized for display — the engine owns that conversion so the
    /// UI never handles a raw path.
    pub path: Arc<str>,
    pub detail: Arc<str>,
}

/// The terminal summary.
#[derive(Clone, Debug)]
pub struct TransferSummary {
    pub kind: TransferKind,
    pub items: u64,
    pub bytes: u64,
    pub skipped: u64,
    pub replaced: u64,
    pub failures: Vec<TransferFailure>,
}

impl TransferSummary {
    pub fn empty(kind: TransferKind) -> Self {
        Self {
            kind,
            items: 0,
            bytes: 0,
            skipped: 0,
            replaced: 0,
            failures: Vec::new(),
        }
    }
}

/// Something that happened to one job. Published on change, never on a timer.
///
/// Periodic readings — bytes so far, throughput, ETA, the item in flight — are
/// deliberately not here. They ride on the scheduler's single subsystem-wide
/// tick as [`Sample`](crate::backend::services::transfer::scheduler::Sample),
/// which is what keeps a 500 MB/s copy from publishing five hundred times a
/// second and what keeps twenty live jobs to five wakeups a second rather than
/// a hundred.
#[derive(Clone, Debug)]
pub enum TransferUpdate {
    /// The lifecycle moved.
    State(TransferState),
    /// One item the job skipped over.
    Failed(TransferFailure),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::services::transfer::conflict::{Conflict, NameClash, Side};

    fn clash() -> Conflict {
        Conflict::NameTaken(NameClash {
            index: 0,
            relative: "a.txt".into(),
            existing_path: "/dest/a.txt".into(),
            source: Side::default(),
            existing: Side::default(),
            keep_both: "a (2).txt".into(),
        })
    }

    /// The requirement this engine was built around: a paused transfer and a
    /// transfer waiting for an answer are different things. A refactor that
    /// merges them breaks here rather than in front of a user whose 50 GB copy
    /// silently needs a click nobody asked for.
    #[test]
    fn the_two_stopped_states_are_not_interchangeable() {
        let paused = TransferState::Paused;
        let waiting = TransferState::WaitingForInput {
            pending: vec![clash()],
        };

        // Different sections of the transfer center...
        assert_eq!(paused.group(), StateGroup::Active);
        assert_eq!(waiting.group(), StateGroup::WaitingForInput);
        assert_ne!(paused.group(), waiting.group());

        // ...and different ways out.
        assert_eq!(paused.resume_path(), ResumePath::Automatic);
        assert_eq!(waiting.resume_path(), ResumePath::Decision);
        assert_ne!(paused.resume_path(), waiting.resume_path());

        // Neither is terminal, and neither is `Pausing`.
        assert!(!paused.is_terminal() && !waiting.is_terminal());
        assert_eq!(TransferState::Pausing.resume_path(), ResumePath::NotStopped);
    }

    #[test]
    fn the_happy_path_walks_queued_to_completed() {
        let walk = [
            TransferState::Queued,
            TransferState::Running,
            TransferState::Pausing,
            TransferState::Paused,
            TransferState::Running,
            TransferState::Completed,
        ];
        let groups: Vec<StateGroup> = walk.iter().map(TransferState::group).collect();
        assert_eq!(
            groups,
            vec![
                StateGroup::Queued,
                StateGroup::Active,
                StateGroup::Active,
                StateGroup::Active,
                StateGroup::Active,
                StateGroup::Completed,
            ]
        );
        assert!(walk[..5].iter().all(TransferState::is_live));
        assert!(!walk[5].is_live());
    }

    #[test]
    fn every_terminal_state_is_terminal() {
        for state in [
            TransferState::Completed,
            TransferState::Failed(Arc::new(TransferError::Cancelled)),
            TransferState::Cancelled,
        ] {
            assert!(state.is_terminal(), "{} is not terminal", state.label());
            assert_eq!(state.group(), StateGroup::Completed);
            assert_eq!(state.resume_path(), ResumePath::Terminal);
        }
    }

    #[test]
    fn priorities_drain_in_declaration_order() {
        let mut all = [
            Priority::Background,
            Priority::Interactive,
            Priority::Normal,
        ];
        all.sort();
        assert_eq!(
            all,
            [
                Priority::Interactive,
                Priority::Normal,
                Priority::Background
            ]
        );
        assert_eq!(Priority::default(), Priority::Normal);
        // The index is what selects the queue, so it must match the order.
        assert_eq!(
            all.map(Priority::index),
            [0, 1, 2],
            "queue indices drifted from the drain order"
        );
    }

    #[test]
    fn only_copy_leaves_the_source_alone() {
        assert!(!TransferKind::Copy.destroys_source());
        assert!(TransferKind::Move.destroys_source());
        assert!(TransferKind::DeletePermanent.destroys_source());
        assert_eq!(TransferKind::Copy.verb(), Some(TransferVerb::Copy));
        assert_eq!(TransferKind::DeletePermanent.verb(), None);
    }

    /// Audit op names are capped at 64 characters by `security::audit`.
    #[test]
    fn audit_op_names_fit_the_audit_record() {
        for kind in [
            TransferKind::Copy,
            TransferKind::Move,
            TransferKind::DeletePermanent,
        ] {
            assert!(kind.audit_op().len() <= 64);
            assert!(kind.audit_op().starts_with("transfer."));
        }
    }

    #[test]
    fn job_ids_are_unique_and_increasing() {
        let a = JobId::next();
        let b = JobId::next();
        assert!(b > a);
        assert_eq!(b.to_string(), format!("{}", b.0));
    }
}
