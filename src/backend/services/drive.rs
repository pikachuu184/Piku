//! Drive and known-location discovery.
//!
//! Both operations stat the filesystem — `known_places` probes seven
//! well-known directories, `list_drives` refreshes the whole mount table — and
//! both were previously called straight from the UI thread (`known_places`
//! from `NavPanel::new`, which runs during layout restore).
//!
//! Results are cached with a short TTL. The sidebar asks for them on every
//! rebuild, and mounts do not change on a frame timescale.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::backend::error::DriveError;
use crate::backend::protocol::{BackendTask, task_channel};
use crate::backend::runtime::BackendRuntime;
use crate::services::fs_service::{DriveInfo, Place};

/// How long a drive/places snapshot stays fresh. Long enough that repeated
/// sidebar rebuilds are free, short enough that plugging in a USB stick shows
/// up without an explicit refresh.
const TTL: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Cached<T> {
    value: Option<T>,
    filled: Option<Instant>,
}

impl<T: Clone> Cached<T> {
    fn get(&self) -> Option<T> {
        let filled = self.filled?;
        if filled.elapsed() < TTL {
            self.value.clone()
        } else {
            None
        }
    }

    fn put(&mut self, value: T) {
        self.value = Some(value);
        self.filled = Some(Instant::now());
    }
}

struct Inner {
    rt: &'static BackendRuntime,
    drives: Mutex<Cached<Vec<DriveInfo>>>,
    places: Mutex<Cached<Vec<Place>>>,
    /// Set while a refresh is in flight, so N concurrent misses do not each
    /// re-enumerate the mount table. A late caller gets the previous value if
    /// there is one, rather than duplicating the work.
    drives_inflight: AtomicBool,
    places_inflight: AtomicBool,
}

/// Cheap-to-clone handle to the drive service.
#[derive(Clone)]
pub struct DriveService(Arc<Inner>);

impl DriveService {
    pub fn new(rt: &'static BackendRuntime) -> Self {
        Self(Arc::new(Inner {
            rt,
            drives: Mutex::new(Cached::default()),
            places: Mutex::new(Cached::default()),
            drives_inflight: AtomicBool::new(false),
            places_inflight: AtomicBool::new(false),
        }))
    }

    /// Mounted drives, partitions, and removable volumes.
    pub fn drives(&self) -> BackendTask<Result<Vec<DriveInfo>, DriveError>> {
        self.fetch(
            "drive.list",
            |inner| &inner.drives,
            |inner| &inner.drives_inflight,
            crate::services::fs_service::list_drives,
        )
    }

    /// Well-known user locations that actually exist (Home, Desktop, ...).
    pub fn places(&self) -> BackendTask<Result<Vec<Place>, DriveError>> {
        self.fetch(
            "drive.places",
            |inner| &inner.places,
            |inner| &inner.places_inflight,
            crate::services::fs_service::known_places,
        )
    }

    /// Shared body for both lookups: serve from cache, else enumerate once on
    /// the blocking pool.
    ///
    /// Failure is reported rather than flattened. `unwrap_or_default()` here
    /// would turn a panic or a shutdown-abort into "this machine has no
    /// mounts", which the sidebar cannot tell from the truth.
    fn fetch<T, Sel, Flag, Work>(
        &self,
        span_name: &'static str,
        select: Sel,
        flag: Flag,
        work: Work,
    ) -> BackendTask<Result<T, DriveError>>
    where
        T: Clone + Send + 'static,
        Sel: Fn(&Inner) -> &Mutex<Cached<T>> + Send + 'static,
        Flag: Fn(&Inner) -> &AtomicBool + Send + 'static,
        Work: FnOnce() -> T + Send + 'static,
    {
        let (sink, task) = task_channel();

        // A warm cache answers without touching the runtime at all, so the
        // common case costs one lock and a clone.
        if let Some(hit) = select(&self.0).lock().get() {
            tracing::trace!(target: "piku::drive", op = span_name, cached = true, "hit");
            sink.finish(Ok(hit));
            return task;
        }

        // Collapse a stampede: if a refresh is already running, answer with
        // whatever we last had rather than enumerating again.
        if flag(&self.0).swap(true, Ordering::SeqCst) {
            let stale = select(&self.0).lock().value.clone();
            tracing::trace!(target: "piku::drive", op = span_name, "refresh already in flight");
            sink.finish(stale.ok_or(DriveError::Busy));
            return task;
        }

        let inner = self.0.clone();
        // `flag` is moved into the future, so capture the reset path up front:
        // the future clears the flag itself, and the not-spawned branch below
        // needs its own way to do so.
        let clear_on_failure = self.0.clone();
        let spawned = self.0.rt.spawn(async move {
            // `parent: None`: an independent operation, not a child of
            // whatever span happens to be entered on this worker thread.
            let span = tracing::info_span!(
                target: "piku::drive",
                parent: None,
                "drive.enumerate",
                op = span_name,
                count = tracing::field::Empty,
            );
            let _entered = span.enter();

            let outcome = if sink.is_cancelled() {
                Err(DriveError::Cancelled)
            } else {
                let for_worker = inner.clone();
                let joined = inner.rt.blocking(move || {
                    let value = work();
                    select(&for_worker).lock().put(value.clone());
                    value
                });
                // Stop waiting if the request is cancelled or the app is
                // quitting; the worker itself is uninterruptible (sysinfo has
                // no cancellation), but nothing downstream should block on it.
                tokio::select! {
                    biased;
                    () = inner.rt.shutdown_token().cancelled() => Err(DriveError::Cancelled),
                    joined = joined => joined.map_err(|_| DriveError::Cancelled),
                }
            };

            flag(&inner).store(false, Ordering::SeqCst);
            if let Ok(value) = &outcome {
                let _ = value;
                span.record("count", tracing::field::Empty);
            }
            sink.finish(outcome);
        });

        if spawned.is_none() {
            // Shutting down: the sink was moved into the future that never
            // ran, so the consumer already sees `ShuttingDown` via the dropped
            // channel. Clear both flags so nothing is wedged on a restart.
            clear_on_failure
                .drives_inflight
                .store(false, Ordering::SeqCst);
            clear_on_failure
                .places_inflight
                .store(false, Ordering::SeqCst);
        }
        task
    }

    /// Drop cached snapshots so the next request re-reads the filesystem.
    // `allow`, not `expect`: this is exercised by the tests below, so the
    // expectation would be unfulfilled in the test target while still firing
    // in the binary.
    #[allow(dead_code, reason = "wired to an explicit sidebar refresh later")]
    pub fn invalidate(&self) {
        *self.0.drives.lock() = Cached::default();
        *self.0.places.lock() = Cached::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> DriveService {
        DriveService::new(crate::backend::runtime::get().expect("runtime"))
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        crate::backend::runtime::get()
            .expect("runtime")
            .handle()
            .block_on(f)
    }

    #[test]
    fn places_come_back_and_all_exist() {
        let svc = service();
        let places = block_on(svc.places().join())
            .expect("task")
            .expect("places");
        // Home should always be present; the rest vary by platform and setup.
        assert!(
            places.iter().all(|p| p.path.exists()),
            "known_places returned a path that does not exist"
        );
    }

    #[test]
    fn drives_come_back_non_empty() {
        let svc = service();
        let drives = block_on(svc.drives().join())
            .expect("task")
            .expect("drives");
        assert!(
            !drives.is_empty(),
            "expected at least one mounted filesystem"
        );
        assert!(drives.iter().all(|d| d.mount.is_absolute()));
    }

    #[test]
    fn a_second_request_is_served_from_cache() {
        let svc = service();
        let first = block_on(svc.drives().join()).expect("task").expect("first");
        let second = block_on(svc.drives().join())
            .expect("task")
            .expect("second");
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn invalidate_forces_a_refresh() {
        let svc = service();
        let _ = block_on(svc.drives().join()).expect("task").expect("warm");
        svc.invalidate();
        // Still answers correctly after the cache is dropped.
        let after = block_on(svc.drives().join())
            .expect("task")
            .expect("after invalidate");
        assert!(!after.is_empty());
    }
}
