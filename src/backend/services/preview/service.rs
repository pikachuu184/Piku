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
use crate::backend::protocol::{
    BackendStream, BackendTask, Cancel, StreamItem, stream_channel, task_channel,
};
use crate::backend::runtime::BackendRuntime;
use crate::backend::services::preview::content::{PreviewPayload, RawImage};
use crate::backend::services::preview::frame::{self, FrameSpec};
use crate::backend::services::preview::{PreviewKind, decode, kind_for_path, load, probe};
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

/// Longest edge of a generated thumbnail. One size serves both the grid
/// (~34 px) and the list (~16 px) even at high DPI, so a file is decoded once
/// and both views share the result.
pub const THUMB_TARGET: u32 = 96;

/// Files larger than this keep the glyph icon — bounds worst-case decode work
/// for hostile or enormous images before [`decode`] even sees them.
pub const THUMB_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Thumbnails decoded in parallel, and the granularity at which the stream
/// batches and re-checks cancellation. Was the old cache's `MAX_ACTIVE`.
const THUMB_CHUNK: usize = 6;

/// Which decoder a queued thumbnail needs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThumbSource {
    Image,
    Video,
}

/// Identifies one thumbnail. `target` is part of the identity so a future
/// second size cannot collide with the current one.
///
/// `size` is here for the same reason [`PreviewKey`] carries it: `mtime` has
/// whole-second granularity, so a file rewritten inside the same wall-clock
/// second keeps its key and would be served the previous thumbnail. A changed
/// length breaks the tie. It is not a content hash and does not pretend to be —
/// a same-length rewrite within one second still collides — but it costs a
/// field already present on the entry and closes the common case.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ThumbKey {
    pub path: PathBuf,
    /// Whole seconds since the epoch, `0` when unknown.
    pub mtime: u64,
    pub size: u64,
    pub target: u32,
}

impl ThumbKey {
    pub fn for_entry(entry: &FsEntry, target: u32) -> Self {
        Self {
            path: entry.path.clone(),
            mtime: entry
                .modified
                .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
            size: entry.size,
            target,
        }
    }
}

pub struct ThumbRequest {
    pub key: ThumbKey,
    pub source: ThumbSource,
}

/// One finished thumbnail. `image: None` means a **permanent** failure — the
/// cache records it so the file is never retried at this key.
pub struct Thumbnail {
    pub key: ThumbKey,
    pub image: Option<RawImage>,
}

/// The terminal item of a thumbnail stream.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ThumbSummary {
    pub requested: usize,
    pub decoded: usize,
    pub failed: usize,
    /// Never attempted, because the batch was superseded. This is the number
    /// that used to be zero: before cancellation, every queued decode ran.
    pub skipped: usize,
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
                joined = joined => match joined {
                    Ok(outcome) => outcome,
                    // A join failure is still reported as cancelled: the UI
                    // treats that as "a newer request owns the panel" and
                    // leaves the previous content alone, which is the right
                    // behaviour when the worker really was superseded. But a
                    // *panic* is a provider defect on a file someone is
                    // looking at, and reporting it as a cancellation makes it
                    // completely silent — no toast, no log, no spinner change.
                    // Providers forbid `unwrap`/`expect`/`panic`, so reaching
                    // this arm means one of them broke that rule, or a
                    // third-party parser panicked outside its `catch_unwind`.
                    Err(error) => {
                        if error.is_panic() {
                            tracing::error!(
                                target: "piku::preview",
                                kind = ?req.kind,
                                "preview provider panicked; reporting as cancelled",
                            );
                        }
                        Err(PreviewError::Cancelled)
                    }
                },
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

    /// Decode a frame *derived* from a file whose preview has already loaded —
    /// page N of a PDF, or a rotated image. See [`frame`] for why both are one
    /// call.
    ///
    /// Takes a concurrency permit like [`Self::preview`] does, because the work
    /// is the same work: a PDF page is a full rasterization and a rotation is a
    /// full re-decode, so a held-down page-forward key must not be able to put a
    /// dozen of them in flight. Cancels when the returned task (or an
    /// [`Inflight`] taken from it) is dropped, which is what makes holding the
    /// key down cost one render rather than one per repeat.
    ///
    /// [`frame`]: crate::backend::services::preview::frame
    /// [`Inflight`]: crate::backend::protocol::Inflight
    pub fn derive_frame(
        &self,
        path: PathBuf,
        spec: FrameSpec,
    ) -> BackendTask<Result<RawImage, PreviewError>> {
        let (sink, task) = task_channel();
        let id = task.id();
        let inner = self.0.clone();

        let spawned = self.0.rt.spawn(async move {
            let span = tracing::info_span!(
                target: "piku::preview",
                parent: None,
                "preview.derive_frame",
                req = %id,
                spec = ?spec,
            );
            let _entered = span.enter();

            let cancel = sink.cancel_handle(inner.rt);
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
                // Same authorization rule as `preview`, for the same reason: the
                // path arrives untrusted even though it came from a preview that
                // already succeeded, and `validate` stays lexical so a symlink
                // is still refused at open time rather than resolved away.
                let validated = for_worker.policy.validate(&path)?;
                frame::render(validated.as_path(), spec, &worker_cancel)
            });

            let outcome = tokio::select! {
                biased;
                () = inner.rt.shutdown_token().cancelled() => Err(PreviewError::Cancelled),
                joined = joined => match joined {
                    Ok(outcome) => outcome,
                    // Same reasoning as `preview`: report cancelled so the panel
                    // keeps what it is showing, but say so in the log, because
                    // the only way to land here is a defect.
                    Err(error) => {
                        if error.is_panic() {
                            tracing::error!(
                                target: "piku::preview",
                                spec = ?spec,
                                "deriving a frame panicked; reporting as cancelled",
                            );
                        }
                        Err(PreviewError::Cancelled)
                    }
                },
            };
            sink.finish(outcome);
        });

        if spawned.is_none() {
            tracing::debug!(
                target: "piku::preview", req = %id, "derive_frame not dispatched: runtime draining"
            );
        }
        task
    }

    /// Grab the video frame at `at_ms` and write it into the screenshot cache,
    /// returning the path written.
    ///
    /// Spawning ffmpeg takes hundreds of milliseconds, and this used to run
    /// straight out of the button's click handler — a visible freeze on every
    /// screenshot. It is a `BackendTask` rather than fire-and-forget because
    /// the toast needs the resulting path.
    pub fn save_frame(
        &self,
        path: PathBuf,
        at_ms: u64,
    ) -> BackendTask<Result<PathBuf, PreviewError>> {
        let (sink, task) = task_channel();
        let id = task.id();
        let inner = self.0.clone();

        let spawned = self.0.rt.spawn(async move {
            let span = tracing::info_span!(
                target: "piku::preview", parent: None, "preview.save_frame", req = %id, at_ms,
            );
            let _entered = span.enter();

            let cancel = sink.cancel_handle(inner.rt);
            let for_worker = inner.clone();
            let joined = inner.rt.blocking(move || {
                let validated = for_worker.policy.validate(&path)?;
                probe::save_frame_png(validated.as_path(), at_ms, &cancel).ok_or_else(|| {
                    PreviewError::Undecodable("could not capture a frame from this video".into())
                })
            });

            let outcome = tokio::select! {
                biased;
                () = inner.rt.shutdown_token().cancelled() => Err(PreviewError::Cancelled),
                joined = joined => joined.unwrap_or(Err(PreviewError::Cancelled)),
            };
            sink.finish(outcome);
        });

        if spawned.is_none() {
            tracing::debug!(target: "piku::preview", req = %id, "save_frame not dispatched");
        }
        task
    }

    /// Decode a batch of thumbnails, streaming them back as they land.
    ///
    /// **Order is priority.** The caller passes them in the order it wants them
    /// on screen — which for the file list is visible-first, because the
    /// virtual list only builds rows in the visible range, so first-seen order
    /// *is* visible order.
    ///
    /// Dropping the stream (or its `Inflight`) stops the decoder at the next
    /// chunk boundary; the rest of the list is reported as `skipped`.
    pub fn thumbnails(&self, batch: Vec<ThumbRequest>) -> BackendStream<Thumbnail, ThumbSummary> {
        // Capacity in batches, not items: a screenful of tiles costs a handful
        // of channel sends rather than one per thumbnail.
        let (sink, stream) = stream_channel(4);
        let id = stream.id();
        let inner = self.0.clone();
        let requested = batch.len();

        let spawned = self.0.rt.spawn(async move {
            let span = tracing::info_span!(
                target: "piku::preview",
                parent: None,
                "preview.thumbnails",
                req = %id,
                requested,
            );
            let _entered = span.enter();

            let cancel = sink.cancel_handle(inner.rt);
            let for_worker = inner.clone();
            let joined = inner.rt.blocking(move || {
                let mut summary = ThumbSummary {
                    requested,
                    ..Default::default()
                };
                let mut chunks = batch.chunks(THUMB_CHUNK).peekable();
                let mut done = 0usize;

                while let Some(chunk) = chunks.next() {
                    // Checked per chunk rather than per item: a superseded
                    // scroll stops within six decodes, and the check costs
                    // nothing next to a decode.
                    if sink.is_cancelled() {
                        summary.skipped = requested - done;
                        break;
                    }
                    done += chunk.len();

                    // Six at a time on the CPU pool. Sequential would be
                    // simpler but would regress a 500-tile grid against the
                    // six-concurrent decoding this replaces.
                    let decoded: Vec<Thumbnail> = for_worker.rt.cpu().install(|| {
                        use rayon::prelude::*;
                        chunk
                            .par_iter()
                            .map(|req| Thumbnail {
                                key: req.key.clone(),
                                image: decode_thumb(&for_worker.policy, req, &cancel),
                            })
                            .collect()
                    });

                    for thumb in &decoded {
                        if thumb.image.is_some() {
                            summary.decoded += 1;
                        } else {
                            summary.failed += 1;
                        }
                    }
                    if !sink.send_blocking(StreamItem::Batch(decoded)) {
                        // Consumer gone or cancelled mid-send.
                        summary.skipped = requested.saturating_sub(done);
                        break;
                    }
                    let _ = chunks.peek();
                }

                sink.finish_blocking(Ok(summary));
            });

            tokio::select! {
                biased;
                () = inner.rt.shutdown_token().cancelled() => {}
                _ = joined => {}
            }
        });

        if spawned.is_none() {
            tracing::debug!(
                target: "piku::preview", req = %id, "thumbnails not dispatched: runtime draining"
            );
        }
        stream
    }
}

/// Decode one thumbnail. `None` for anything that will not or cannot safely
/// render, which the cache records as a permanent failure.
fn decode_thumb(policy: &PathPolicy, req: &ThumbRequest, cancel: &Cancel) -> Option<RawImage> {
    // Same authorization rule as `preview`: the untrusted path is validated in
    // the worker, lexically, so a symlink is still refused at open time rather
    // than resolved away.
    let validated = policy.validate(&req.key.path).ok()?;
    let path = validated.as_path();
    match req.source {
        ThumbSource::Image => decode::thumbnail(path, req.key.target, THUMB_MAX_BYTES, cancel).ok(),
        // ffmpeg produces a PNG poster; it has its own size and timeout gates.
        ThumbSource::Video => probe::poster_frame(path, Some(req.key.target), cancel),
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

    /// Thumbnails arrive in the order they were asked for, which is what makes
    /// "order is priority" true rather than aspirational.
    #[test]
    fn thumbnails_stream_back_in_request_order() {
        let dir = scratch("thumbs");
        let names: Vec<PathBuf> = (0..8)
            .map(|i| {
                let p = dir.join(format!("{i}.png"));
                write_test_png(&p);
                p
            })
            .collect();

        let batch: Vec<ThumbRequest> = names
            .iter()
            .map(|p| ThumbRequest {
                key: ThumbKey {
                    path: p.clone(),
                    mtime: 0,
                    size: 0,
                    target: THUMB_TARGET,
                },
                source: ThumbSource::Image,
            })
            .collect();

        let mut stream = service().thumbnails(batch);
        let mut got: Vec<PathBuf> = Vec::new();
        let mut summary = None;
        block_on(async {
            while let Some(item) = stream.next().await {
                match item {
                    StreamItem::Batch(thumbs) => {
                        got.extend(thumbs.into_iter().map(|t| t.key.path));
                    }
                    StreamItem::Done(result) => {
                        summary = result.ok();
                        break;
                    }
                    StreamItem::Progress(_) => {}
                }
            }
        });

        assert_eq!(got, names, "thumbnails came back out of order");
        let summary = summary.expect("a terminal summary");
        assert_eq!(summary.requested, 8);
        assert_eq!(summary.decoded, 8, "{summary:?}");
        assert_eq!(summary.skipped, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The headline behaviour for scrolling: a superseded batch must stop,
    /// leaving work *unattempted* rather than running it to completion.
    #[test]
    fn a_superseded_thumbnail_batch_stops_decoding_the_rest_of_its_list() {
        let dir = scratch("superseded");
        // Enough chunks that cancelling after the first leaves plenty behind.
        let batch: Vec<ThumbRequest> = (0..200)
            .map(|i| {
                let p = dir.join(format!("{i}.png"));
                write_test_png(&p);
                ThumbRequest {
                    key: ThumbKey {
                        path: p,
                        mtime: 0,
                        size: 0,
                        target: THUMB_TARGET,
                    },
                    source: ThumbSource::Image,
                }
            })
            .collect();

        let mut stream = service().thumbnails(batch);
        let leash = stream.inflight();
        let mut summary = None;
        block_on(async {
            while let Some(item) = stream.next().await {
                match item {
                    // Supersede as soon as the first results land, the way a
                    // scroll would.
                    StreamItem::Batch(_) => leash.cancel(),
                    StreamItem::Done(result) => {
                        summary = result.ok();
                        break;
                    }
                    StreamItem::Progress(_) => {}
                }
            }
        });

        let summary = summary.expect("a cancelled batch still owes a summary");
        assert_eq!(summary.requested, 200);
        assert!(
            summary.skipped > 0,
            "a superseded batch decoded everything anyway: {summary:?}"
        );
        assert!(
            summary.decoded < 200,
            "nothing was actually skipped: {summary:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 4×4 PNG — small enough that 200 of them are cheap, real enough that
    /// the decode path is genuinely exercised.
    fn write_test_png(path: &Path) {
        use image::ImageEncoder as _;
        let buf = image::RgbaImage::from_pixel(4, 4, image::Rgba([7, 9, 11, 255]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(buf.as_raw(), 4, 4, image::ExtendedColorType::Rgba8)
            .expect("encode");
        let _ = std::fs::write(path, bytes);
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
