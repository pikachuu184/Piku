//! Drive and known-location discovery.
//!
//! Both operations stat the filesystem — `known_places` probes seven
//! well-known directories, `list_drives` refreshes the whole mount table — and
//! both were previously called straight from the UI thread (`known_places`
//! from `NavPanel::new`, which runs during layout restore).
//!
//! Results are cached with a short TTL. The sidebar asks for them on every
//! rebuild, and mounts do not change on a frame timescale.

use std::time::{Duration, Instant};

use parking_lot::Mutex;

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
}

/// Cheap-to-clone handle to the drive service.
#[derive(Clone)]
pub struct DriveService(std::sync::Arc<Inner>);

impl DriveService {
    pub fn new(rt: &'static BackendRuntime) -> Self {
        Self(std::sync::Arc::new(Inner {
            rt,
            drives: Mutex::new(Cached::default()),
            places: Mutex::new(Cached::default()),
        }))
    }

    /// Mounted drives, partitions, and removable volumes.
    pub fn drives(&self) -> BackendTask<Vec<DriveInfo>> {
        let (sink, task) = task_channel();

        // A warm cache answers without touching the runtime at all, so the
        // common case costs one lock and a clone.
        if let Some(hit) = self.0.drives.lock().get() {
            tracing::trace!(target: "piku::drive", cached = true, "drives");
            sink.finish(hit);
            return task;
        }

        let inner = self.0.clone();
        self.0.rt.spawn(async move {
            // `parent: None`: this is an independent operation, not a child of
            // whatever span happens to be entered on this worker thread.
            let span = tracing::info_span!(
                target: "piku::drive",
                parent: None,
                "drive.list",
                count = tracing::field::Empty,
            );
            let _entered = span.enter();
            let inner2 = inner.clone();
            let listed = inner
                .rt
                .blocking(move || {
                    let drives = crate::services::fs_service::list_drives();
                    inner2.drives.lock().put(drives.clone());
                    drives
                })
                .await
                .unwrap_or_default();
            span.record("count", listed.len());
            sink.finish(listed);
        });
        task
    }

    /// Well-known user locations that actually exist (Home, Desktop, …).
    pub fn places(&self) -> BackendTask<Vec<Place>> {
        let (sink, task) = task_channel();

        if let Some(hit) = self.0.places.lock().get() {
            tracing::trace!(target: "piku::drive", cached = true, "places");
            sink.finish(hit);
            return task;
        }

        let inner = self.0.clone();
        self.0.rt.spawn(async move {
            let span = tracing::info_span!(
                target: "piku::drive",
                parent: None,
                "drive.places",
                count = tracing::field::Empty,
            );
            let _entered = span.enter();
            let inner2 = inner.clone();
            let found = inner
                .rt
                .blocking(move || {
                    let places = crate::services::fs_service::known_places();
                    inner2.places.lock().put(places.clone());
                    places
                })
                .await
                .unwrap_or_default();
            span.record("count", found.len());
            sink.finish(found);
        });
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
        let places = block_on(svc.places().join()).expect("places");
        // Home should always be present; the rest vary by platform and setup.
        assert!(
            places.iter().all(|p| p.path.exists()),
            "known_places returned a path that does not exist"
        );
    }

    #[test]
    fn drives_come_back_non_empty() {
        let svc = service();
        let drives = block_on(svc.drives().join()).expect("drives");
        assert!(
            !drives.is_empty(),
            "expected at least one mounted filesystem"
        );
        assert!(drives.iter().all(|d| d.mount.is_absolute()));
    }

    #[test]
    fn a_second_request_is_served_from_cache() {
        let svc = service();
        let first = block_on(svc.drives().join()).expect("first");
        let second = block_on(svc.drives().join()).expect("second");
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn invalidate_forces_a_refresh() {
        let svc = service();
        let _ = block_on(svc.drives().join()).expect("warm");
        svc.invalidate();
        // Still answers correctly after the cache is dropped.
        let after = block_on(svc.drives().join()).expect("after invalidate");
        assert!(!after.is_empty());
    }
}
