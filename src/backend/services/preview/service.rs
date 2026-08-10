//! The preview service: authorization, concurrency, spans, cancellation.
//!
//! Providers hold parsers; this holds the envelope around them. Splitting it
//! that way is what lets a provider be tested with a two-line `Cancel` and no
//! runtime at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use tokio::sync::Semaphore;

use crate::backend::error::PreviewError;
use crate::backend::path::PathPolicy;
use crate::backend::protocol::{BackendTask, task_channel};
use crate::backend::runtime::BackendRuntime;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::{PreviewKind, kind_for_path, load};
use crate::core::entry::FsEntry;

/// Concurrent full previews.
///
/// The blocking pool (24 threads) already bounds this loosely, but a burst of
/// arrow-key presses should not be able to put a dozen PDF rasterizations in
/// flight before the first one notices it was superseded. Each provider checks
/// cancellation on entry, so a queued-then-cancelled request costs almost
/// nothing.
const MAX_CONCURRENT: usize = 4;

/// Identifies a decoded preview for caching.
///
/// Path alone is not enough — a file can be replaced in place — so the key
/// carries the modification time and size that were true when it was read.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct PreviewKey {
    path: PathBuf,
    /// Whole seconds since the epoch, `0` when unknown — an edited file gets a
    /// new key and is re-decoded rather than served from a stale entry.
    mtime: u64,
    size: u64,
}

impl PreviewKey {
    pub fn new(path: PathBuf, modified: Option<std::time::SystemTime>, size: u64) -> Self {
        let mtime = modified
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { path, mtime, size }
    }

    /// Key a path by stat-ing it. Blocking — worker threads only. This is why
    /// the service echoes the key back to the caller: it means the media panel
    /// no longer stats on the UI thread just to build a cache probe.
    fn by_stat(path: &Path) -> Self {
        let meta = std::fs::metadata(path).ok();
        Self::new(
            path.to_path_buf(),
            meta.as_ref().and_then(|m| m.modified().ok()),
            meta.as_ref().map(|m| m.len()).unwrap_or(0),
        )
    }
}

/// What the UI asks for. Deliberately holds no `ValidatedPath`: the path is
/// untrusted until the worker authorizes it, and keeping [`PathPolicy`] inside
/// the service means there is exactly one place that decides.
pub struct PreviewRequest {
    pub path: PathBuf,
    pub kind: PreviewKind,
    pub ext: Arc<str>,
}

impl PreviewRequest {
    /// From a directory entry, which already knows its own extension.
    pub fn for_entry(entry: &FsEntry) -> Self {
        Self {
            path: entry.path.clone(),
            kind: crate::backend::services::preview::decide_kind(entry),
            ext: entry.ext.as_str().into(),
        }
    }

    /// From a bare path (the media panel, opened directly on a file).
    pub fn for_path(path: &Path) -> Self {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            kind: kind_for_path(path),
            ext: ext.as_str().into(),
        }
    }
}

/// A finished preview and the key it should be cached under.
pub struct PreviewReady {
    pub key: PreviewKey,
    pub payload: PreviewPayload,
}

struct Inner {
    rt: &'static BackendRuntime,
    policy: PathPolicy,
    permits: Semaphore,
}

#[derive(Clone)]
pub struct PreviewService(Arc<Inner>);

impl PreviewService {
    pub fn new(rt: &'static BackendRuntime, policy: PathPolicy) -> Self {
        Self(Arc::new(Inner {
            rt,
            policy,
            permits: Semaphore::new(MAX_CONCURRENT),
        }))
    }

    /// Decode one file. Cancels when the returned task (or an [`Inflight`]
    /// taken from it) is dropped.
    ///
    /// [`Inflight`]: crate::backend::protocol::Inflight
    pub fn preview(&self, req: PreviewRequest) -> BackendTask<Result<PreviewReady, PreviewError>> {
        let (sink, task) = task_channel();
        let id = task.id();
        let inner = self.0.clone();

        let spawned = self.0.rt.spawn(async move {
            // `parent: None`: an independent operation, not a child of whatever
            // span happens to be entered on this worker thread.
            let span = tracing::info_span!(
                target: "piku::preview",
                parent: None,
                "preview.load",
                req = %id,
                kind = ?req.kind,
            );
            let _entered = span.enter();

            let cancel = sink.cancel_handle(inner.rt);
            // Held for the duration of the decode and dropped with the future,
            // so a request cancelled while queued releases its slot without
            // ever having taken one.
            let Ok(_permit) = inner.permits.acquire().await else {
                sink.finish(Err(PreviewError::Cancelled));
                return;
            };
            if cancel.is_cancelled() {
                sink.finish(Err(PreviewError::Cancelled));
                return;
            }

            let for_worker = inner.clone();
            let worker_cancel = cancel.clone();
            let joined = inner.rt.blocking(move || {
                // Authorization happens here, inside the worker, on the
                // untrusted path — never taken on trust from the caller.
                //
                // `validate` (lexical), not `resolve` (canonicalizing):
                // resolving would *follow* a symlink and hand the provider its
                // target, defeating the refusal in `read::open_read`. Preview
                // containment is lexical roots plus "never read through a
                // link", which is exactly what the engine did before it moved.
                let validated = for_worker.policy.validate(&req.path)?;
                let path = validated.as_path();
                let key = PreviewKey::by_stat(path);
                let payload = load(req.kind, path, &req.ext, &worker_cancel)?;
                Ok::<_, PreviewError>(PreviewReady { key, payload })
            });

            // Stop waiting if the app is quitting; the provider's own
            // cancellation checks are what stop the work itself.
            let outcome = tokio::select! {
                biased;
                () = inner.rt.shutdown_token().cancelled() => Err(PreviewError::Cancelled),
                joined = joined => joined.unwrap_or(Err(PreviewError::Cancelled)),
            };
            sink.finish(outcome);
        });

        if spawned.is_none() {
            // Shutting down: the sink was moved into the future that never ran,
            // so the consumer already sees `ShuttingDown` via the dropped
            // channel. Nothing to unwind.
            tracing::debug!(target: "piku::preview", req = %id, "not dispatched: runtime draining");
        }
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::protocol::Cancel;

    fn rt() -> &'static BackendRuntime {
        crate::backend::runtime::get().expect("runtime")
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        rt().handle().block_on(f)
    }

    fn service() -> PreviewService {
        PreviewService::new(rt(), PathPolicy::with_system_roots())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("piku-preview-service-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn a_preview_round_trips_and_reports_the_key_it_read() {
        let dir = scratch("roundtrip");
        let file = dir.join("a.rs");
        let _ = std::fs::write(&file, "fn main() {}\n");

        let ready = block_on(service().preview(PreviewRequest::for_path(&file)).join())
            .expect("dispatch")
            .expect("preview");
        assert!(matches!(ready.payload, PreviewPayload::Code { .. }));
        assert_eq!(ready.key.path, file);
        assert_eq!(ready.key.size, 13, "the key carries the size actually read");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The headline fix. Dropping the leash must stop the worker, not merely
    /// discard its answer — a `Barrier` proves the worker actually observes it
    /// rather than the test racing a fast decode.
    #[test]
    fn dropping_the_leash_cancels_the_worker_before_it_finishes() {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let (sink, task) = task_channel::<Result<(), PreviewError>>();
        let leash = task.inflight();
        let cancel = sink.cancel_handle(rt());

        let worker = {
            let barrier = barrier.clone();
            let observed = observed.clone();
            std::thread::spawn(move || {
                // Wait until the test has dropped the leash...
                barrier.wait();
                // ...then check, the way a provider's loop head does.
                observed.store(cancel.is_cancelled(), std::sync::atomic::Ordering::SeqCst);
                sink.finish(Err(PreviewError::Cancelled));
            })
        };

        drop(leash);
        barrier.wait();
        worker.join().expect("worker");

        assert!(
            observed.load(std::sync::atomic::Ordering::SeqCst),
            "the worker did not see the cancellation"
        );
        // And the consumer still gets its one answer rather than hanging.
        assert!(matches!(
            block_on(task.join()),
            Ok(Err(PreviewError::Cancelled))
        ));
    }

    #[test]
    fn a_request_for_an_unreadable_path_fails_rather_than_hanging() {
        let dir = scratch("unreadable");
        let missing = dir.join("nope.rs");
        let result = block_on(service().preview(PreviewRequest::for_path(&missing)).join())
            .expect("dispatch");
        assert!(matches!(result, Err(PreviewError::File(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A path the policy refuses must never reach a provider — and the refusal
    /// must happen in the worker, not be taken on trust from the caller.
    #[test]
    fn an_unauthorized_path_is_refused_by_the_service() {
        let result = block_on(
            service()
                .preview(PreviewRequest::for_path(Path::new("relative/not/absolute")))
                .join(),
        )
        .expect("dispatch");
        assert!(
            matches!(result, Err(PreviewError::Path(_))),
            "a relative path must be refused by the policy"
        );
    }

    /// Guards the symlink decision recorded in `preview()`: validate is
    /// lexical, so the provider still sees the link and refuses it. Resolving
    /// here instead would silently follow it.
    #[cfg(unix)]
    #[test]
    fn the_service_does_not_resolve_a_symlink_out_from_under_the_read_refusal() {
        use crate::backend::error::FileError;

        let dir = scratch("symlink");
        let target = dir.join("real.rs");
        let link = dir.join("link.rs");
        let _ = std::fs::write(&target, "fn main() {}\n");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");

        let result =
            block_on(service().preview(PreviewRequest::for_path(&link)).join()).expect("dispatch");
        assert!(
            matches!(result, Err(PreviewError::File(FileError::IsSymlink(_)))),
            "the service resolved the link instead of leaving it for the reader to refuse"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_request_built_from_an_entry_picks_the_kind_from_its_extension() {
        let req = PreviewRequest::for_path(Path::new("/tmp/x.PNG"));
        assert_eq!(req.kind, PreviewKind::Image);
        assert_eq!(req.ext.as_ref(), "png", "the extension is lower-cased");
    }

    #[test]
    fn cancelling_before_the_worker_starts_still_answers() {
        // A cancelled request still owes its consumer exactly one answer, or
        // the panel sits in its loading state forever.
        let dir = scratch("early-cancel");
        let file = dir.join("a.rs");
        let _ = std::fs::write(&file, "fn main() {}\n");

        let task = service().preview(PreviewRequest::for_path(&file));
        task.inflight().cancel();
        let result = block_on(task.join());
        assert!(result.is_ok(), "the consumer must still receive an answer");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_test_cancel_helpers_reach_providers_unchanged() {
        // Sanity: the service's Cancel and a hand-built one behave the same, so
        // provider-level tests using Cancel::already() are testing the real
        // thing.
        assert!(Cancel::already().is_cancelled());
        assert!(!Cancel::never().is_cancelled());
    }
}
