//! The asynchronous filesystem backend.
//!
//! The UI never calls the operating system. It dispatches high-level commands
//! to the services behind [`Backend`] and renders whatever view model comes
//! back; the render thread stays free regardless of how slow the disk, the
//! network share, or the removable medium turns out to be.
//!
//! # Layout
//!
//! * [`runtime`] — the Tokio runtime, the rayon pool, and shutdown.
//! * [`protocol`] — `BackendTask` / `BackendStream` / `Inflight`, the only two
//!   response shapes and the only cancellation mechanism.
//! * [`error`] — one `thiserror` enum per service, composing into
//!   `BackendError`.
//! * [`services`] — the services themselves. **Nothing under `services/` may
//!   import gpui**; `ci/invariants.sh` enforces it, which is what keeps the
//!   backend portable and testable without a window.
//! * [`dispatch`] — the one file here that does know about gpui: the bridge
//!   that lands results in an `Entity` and calls `notify`.

pub mod dispatch;
pub mod error;
pub mod path;
pub mod protocol;
pub mod runtime;
pub mod services;

use std::sync::Arc;

use crate::backend::path::PathPolicy;
use crate::backend::runtime::BackendRuntime;
use crate::backend::services::drive::DriveService;
use crate::backend::services::preview::PreviewService;
use crate::backend::services::transfer::TransferService;

struct BackendInner {
    rt: &'static BackendRuntime,
    drive: DriveService,
    preview: PreviewService,
    transfer: TransferService,
}

/// Cheap-to-clone handle to every backend service.
///
/// Held by `PikuState`, so any view can reach it. Services do not hold a
/// `Backend` — cross-service dependencies are passed explicitly at
/// construction, which keeps the graph acyclic.
#[derive(Clone)]
pub struct Backend(Arc<BackendInner>);

impl Backend {
    /// Start the backend. Returns `None` if the runtime could not be built, in
    /// which case the app should surface the failure rather than run without a
    /// backend.
    pub fn new() -> Option<Self> {
        let rt = runtime::get()?;
        // One policy, built once and shared: `with_system_roots` probes drive
        // letters on Windows, which is not something to redo per service, let
        // alone per request.
        let policy = PathPolicy::with_system_roots();
        Some(Self(Arc::new(BackendInner {
            rt,
            drive: DriveService::new(rt),
            preview: PreviewService::new(rt, policy.clone()),
            // No post-transfer sinks yet, which makes the pipeline a null check.
            // Indexing, persistence, and thumbnails register here when they land.
            transfer: TransferService::new(rt, policy, Vec::new()),
        })))
    }

    pub fn drive(&self) -> &DriveService {
        &self.0.drive
    }

    pub fn preview(&self) -> &PreviewService {
        &self.0.preview
    }

    pub fn transfer(&self) -> &TransferService {
        &self.0.transfer
    }

    // `allow`, not `expect`: used by the tests below but not yet by the
    // binary, so an expectation would be unfulfilled in the test target.
    #[allow(dead_code, reason = "services take the runtime directly until Stage 3")]
    pub fn runtime(&self) -> &'static BackendRuntime {
        self.0.rt
    }

    /// Cancel outstanding work and wait briefly for it to stop. Called once,
    /// on application quit.
    ///
    /// `async` rather than blocking: gpui polls quit futures on the foreground
    /// executor and enforces its own 200 ms timeout there, so blocking would
    /// freeze the window and defeat that timeout.
    pub async fn shutdown(&self) -> bool {
        // Transfers first, and explicitly. The runtime's own token would reach
        // them a moment later, but a copy's cancel flag is polled by a blocking
        // thread between 1 MB chunks, so it needs the whole grace period — not
        // whatever is left of it. Cancelling here is also what leaves no partial
        // destination file behind: the chunk loop unwinds and removes it, which a
        // killed thread could not do.
        self.0.transfer.shutdown();
        self.0.rt.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_starts_and_exposes_its_services() {
        let backend = Backend::new().expect("backend starts");
        // Cloning is cheap and shares the same services.
        let clone = backend.clone();
        assert!(std::ptr::eq(backend.runtime(), clone.runtime()));
    }
}
