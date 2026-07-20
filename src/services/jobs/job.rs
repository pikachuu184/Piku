//! Background job model: every mutating operation is a queued job with its
//! own progress, status, and cancellation flag.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Copy,
    Move,
    Delete,
    Rename,
    NewFolder,
}

impl JobKind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Copy => "Copying",
            Self::Move => "Moving",
            Self::Delete => "Deleting",
            Self::Rename => "Renaming",
            Self::NewFolder => "Creating folder",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: u64,
    #[allow(dead_code)]
    pub kind: JobKind,
    pub title: String,
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: usize,
    pub done_items: usize,
    pub status: JobStatus,
    pub cancel: Arc<AtomicBool>,
}

impl Job {
    pub fn new(id: u64, kind: JobKind, title: String) -> Self {
        Self {
            id,
            kind,
            title,
            total_bytes: 0,
            done_bytes: 0,
            total_items: 0,
            done_items: 0,
            status: JobStatus::Running,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == JobStatus::Running
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
}

/// Events streamed from the worker thread back to the UI executor.
pub enum JobEvent {
    Scanned { total_bytes: u64, total_items: usize },
    Progress { delta_bytes: u64, delta_items: usize },
    Finished(Result<String, String>),
}
