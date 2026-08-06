//! Ownership of the backend's execution resources: one Tokio runtime for
//! orchestration and blocking I/O, one rayon pool for CPU-bound fan-out, and
//! the shutdown plumbing that drains the Tokio side.
//!
//! # Why Tokio at all
//!
//! Not for async I/O. There is no true asynchronous local filesystem —
//! `tokio::fs` is `spawn_blocking` underneath — so the runtime is configured
//! with no IO driver at all. What it provides that gpui's executor does not:
//! `CancellationToken`, `Semaphore`, bounded `mpsc` (backpressure), timers,
//! and `TaskTracker` for a graceful drain. Real work runs on `spawn_blocking`
//! (once per *logical operation*, not per syscall) or on the rayon pool.
//!
//! # Why the runtime is never dropped
//!
//! Dropping a Tokio `Runtime` from inside an async context panics, and PIKU's
//! teardown (`cx.on_release` → `cx.quit()`) runs on the gpui main thread while
//! background tasks may still be alive. The runtime therefore lives in a
//! `OnceLock` static and leaks at process exit, which is free. Orderly
//! shutdown is [`BackendRuntime::shutdown`]: cancel, close, drain with a
//! deadline.

// Parts of this surface (the rayon pool, the shutdown token) are used by
// services that land in Stages 3-5. `expect` rather than `allow`: it starts
// erroring once everything is live, which is the reminder to delete it.
#![expect(dead_code)]

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// How long the shutdown drain waits for in-flight work to notice cancellation
/// before giving up and letting the process exit anyway.
///
/// This **must** stay well under gpui's own `App::SHUTDOWN_TIMEOUT` (200 ms),
/// which is enforced with `foreground_executor.block_with_timeout(..)` around
/// the quit futures. Overrunning it means a frozen window and a spurious
/// "timed out waiting on app_will_quit"; on macOS and Windows the platform may
/// kill the process inside that window.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(100);

/// Tokio worker threads. These only orchestrate — every blocking call is
/// handed to the blocking pool — so two is ample and keeps the thread count
/// of a desktop app honest.
const WORKER_THREADS: usize = 2;

/// Cap on concurrent blocking operations. Tokio's default is 512, which for
/// filesystem work means 512 threads all seeking the same disk. This is a
/// queue depth, not a parallelism target.
const MAX_BLOCKING_THREADS: usize = 24;

pub struct BackendRuntime {
    rt: Runtime,
    cpu: rayon::ThreadPool,
    tracker: TaskTracker,
    shutdown: CancellationToken,
    /// Set once shutdown begins. `TaskTracker::close()` deliberately does not
    /// prevent further spawns, so without this a dispatch arriving after quit
    /// would be registered on a closed tracker and never drained.
    shutting_down: AtomicBool,
}

/// `Option` inside the cell so a failed build is memoized: without it, every
/// call would retry a build that is not going to start succeeding.
static RUNTIME: OnceLock<Option<BackendRuntime>> = OnceLock::new();

/// The process-wide backend runtime, started on first use.
///
/// Returns `None` if the runtime could not be built — the caller degrades
/// rather than panicking, since a file manager that cannot start its thread
/// pools should still be able to say so.
pub fn get() -> Option<&'static BackendRuntime> {
    // `get_or_init`, not `get`-then-`set`: the losing side of a `set` race
    // would **drop** a fully-built `Runtime`, and dropping a runtime from an
    // async context panics — the exact hazard this module's header warns
    // about. `get_or_init` guarantees only one is ever built.
    RUNTIME
        .get_or_init(|| match BackendRuntime::build() {
            Ok(built) => Some(built),
            Err(error) => {
                tracing::error!(%error, "could not start the backend runtime");
                None
            }
        })
        .as_ref()
}

impl BackendRuntime {
    fn build() -> anyhow::Result<Self> {
        let rt = Builder::new_multi_thread()
            .worker_threads(WORKER_THREADS)
            .max_blocking_threads(MAX_BLOCKING_THREADS)
            .thread_name("piku-bk")
            .thread_stack_size(1 << 20)
            // No `enable_io`: PIKU opens no sockets. `enable_time` is needed
            // for the debounce and batching timers.
            .enable_time()
            .build()?;

        let cpu = rayon::ThreadPoolBuilder::new()
            .num_threads(cpu_threads())
            .thread_name(|i| format!("piku-cpu{i}"))
            .build()?;

        tracing::debug!(
            workers = WORKER_THREADS,
            max_blocking = MAX_BLOCKING_THREADS,
            cpu_threads = cpu.current_num_threads(),
            "backend runtime started"
        );

        Ok(Self {
            rt,
            cpu,
            tracker: TaskTracker::new(),
            shutdown: CancellationToken::new(),
            shutting_down: AtomicBool::new(false),
        })
    }

    /// Whether shutdown has begun. Work submitted after this point is refused
    /// rather than queued onto a tracker nobody will wait on.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// A handle for spawning onto the runtime.
    pub fn handle(&self) -> &Handle {
        self.rt.handle()
    }

    /// The rayon pool for CPU-bound fan-out.
    pub fn cpu(&self) -> &rayon::ThreadPool {
        &self.cpu
    }

    /// Cancelled when the application is shutting down. Long-running work
    /// should select on this in addition to its own per-request token.
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    /// Spawn a future, tracked so [`shutdown`](Self::shutdown) can wait for it.
    ///
    /// Returns `None` once shutdown has begun; the caller's response half is
    /// then dropped, which surfaces to the UI as `BackendError::ShuttingDown`.
    pub fn spawn<F>(&self, future: F) -> Option<JoinHandle<F::Output>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        if self.is_shutting_down() {
            return None;
        }
        Some(self.tracker.spawn_on(future, self.rt.handle()))
    }

    /// Run a blocking closure on the blocking pool, **carrying the current
    /// tracing span into the worker**.
    ///
    /// `spawn_blocking` does not propagate spans; without the re-entry below,
    /// every filesystem operation would log detached from the request that
    /// asked for it. This is the only place in the crate allowed to call
    /// `spawn_blocking` — `ci/invariants.sh` enforces that.
    pub fn blocking<F, R>(&self, f: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let span = tracing::Span::current();
        self.rt.handle().spawn_blocking(move || {
            let _entered = span.enter();
            let _blocking = BlockingScope::enter();
            f()
        })
    }

    /// Run a CPU-bound closure on the rayon pool, carrying the current span.
    ///
    /// Never take a lock inside `f` that another rayon task may already hold —
    /// the pool has no work-stealing escape from a deadlock.
    ///
    /// **Not covered by [`shutdown`](Self::shutdown).** rayon work is not
    /// tracked and is killed by process exit; that is acceptable because every
    /// CPU task here is a pure computation (hashing, decoding) with no
    /// half-finished filesystem state to leave behind. Anything that mutates
    /// the filesystem must go through [`blocking`](Self::blocking) instead.
    pub fn cpu_spawn<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let span = tracing::Span::current();
        self.cpu.spawn(move || {
            let _entered = span.enter();
            f();
        });
    }

    /// Signal every tracked task to stop, then wait up to [`SHUTDOWN_GRACE`]
    /// for them to finish. Returns whether the drain completed in time.
    ///
    /// This is an `async fn` on purpose. The obvious implementation —
    /// `rt.block_on(tracker.wait())` — is called from gpui's quit handler,
    /// which runs on the **foreground executor**; blocking there stops gpui
    /// from enforcing its own 200 ms shutdown timeout, so the timeout cannot
    /// fire until we return and the window is frozen for the whole grace
    /// period. Awaiting instead yields the executor back.
    pub async fn shutdown(&self) -> bool {
        self.begin_shutdown();
        let drained = tokio::time::timeout(SHUTDOWN_GRACE, self.tracker.wait())
            .await
            .is_ok();
        self.report_drain(drained);
        drained
    }

    /// Synchronous drain, for callers that are not already async (tests, and
    /// any non-gpui entry point).
    ///
    /// Panics are avoided rather than risked: `block_on` from inside a runtime
    /// thread would panic, so that case degrades to cancel-without-waiting.
    pub fn shutdown_blocking(&self) -> bool {
        self.begin_shutdown();
        if Handle::try_current().is_ok() {
            // Already inside a runtime — blocking here would panic. The cancel
            // has landed; skip the wait.
            tracing::debug!("shutdown_blocking called from a runtime thread; not waiting");
            return false;
        }
        let drained = self.rt.block_on(async {
            tokio::time::timeout(SHUTDOWN_GRACE, self.tracker.wait())
                .await
                .is_ok()
        });
        self.report_drain(drained);
        drained
    }

    fn begin_shutdown(&self) {
        // Ordered so that a task spawned concurrently either sees the flag and
        // is refused, or is registered with the tracker and gets waited on.
        self.shutting_down.store(true, Ordering::SeqCst);
        self.shutdown.cancel();
        self.tracker.close();
    }

    fn report_drain(&self, drained: bool) {
        if !drained {
            tracing::warn!(
                grace_ms = SHUTDOWN_GRACE.as_millis() as u64,
                "backend tasks did not drain before the deadline"
            );
        }
    }
}

thread_local! {
    /// True while a closure passed to [`BackendRuntime::blocking`] is running.
    static IN_BLOCKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the caller is inside a sanctioned blocking context — that is,
/// inside a closure handed to [`BackendRuntime::blocking`].
///
/// The protocol's `*_blocking` senders assert on this. Checking our own marker
/// is precise: Tokio exposes no "is blocking allowed here" predicate, and
/// `Handle::try_current()` succeeds in `spawn_blocking` workers *and* in async
/// tasks, so it cannot distinguish the two.
pub fn in_blocking_context() -> bool {
    IN_BLOCKING.with(std::cell::Cell::get)
}

/// Marks the thread as being inside `runtime.blocking(..)` until dropped.
/// Restores rather than clears, so nesting is safe.
struct BlockingScope(bool);

impl BlockingScope {
    fn enter() -> Self {
        Self(IN_BLOCKING.with(|f| f.replace(true)))
    }
}

impl Drop for BlockingScope {
    fn drop(&mut self) {
        IN_BLOCKING.with(|f| f.set(self.0));
    }
}

/// Threads for the CPU pool: one per core, capped so a 64-core workstation
/// does not spawn 64 image decoders for a directory of thumbnails.
fn cpu_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn the_runtime_starts_and_is_a_singleton() {
        let a = get().expect("runtime starts");
        let b = get().expect("runtime is cached");
        assert!(std::ptr::eq(a, b));
        assert!(a.cpu().current_num_threads() >= 1);
    }

    #[test]
    fn blocking_work_runs_and_returns_its_value() {
        let rt = get().expect("runtime");
        let out = rt.rt.block_on(rt.blocking(|| 6 * 7)).expect("join");
        assert_eq!(out, 42);
    }

    /// Records the names of spans entered on any thread.
    ///
    /// A subscriber is required for this test to mean anything: without one,
    /// every `Span` is disabled and `Span::current()` is `none()`, so a naive
    /// "is there a current span?" assertion passes vacuously whether or not
    /// propagation works.
    #[derive(Clone, Default)]
    struct SpanRecorder(Arc<parking_lot::Mutex<Vec<String>>>);

    impl<S> tracing_subscriber::Layer<S> for SpanRecorder
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    {
        fn on_enter(&self, id: &tracing::span::Id, ctx: tracing_subscriber::layer::Context<'_, S>) {
            if let Some(span) = ctx.span(id) {
                self.0.lock().push(span.name().to_string());
            }
        }
    }

    #[test]
    fn blocking_work_inherits_the_callers_span() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let rt = get().expect("runtime");
        let recorder = SpanRecorder::default();
        let subscriber = tracing_subscriber::registry().with(recorder.clone());

        // A `Span` captures its dispatcher, so entering it on the worker
        // thread reaches this subscriber even though the default is
        // thread-local to the caller.
        let worker_thread = tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("caller");
            let _entered = span.enter();
            rt.rt
                .block_on(rt.blocking(|| std::thread::current().id()))
                .expect("join")
        });

        // The work really did leave this thread...
        assert_ne!(
            worker_thread,
            std::thread::current().id(),
            "blocking work ran on the calling thread"
        );
        // ...and `caller` was entered twice: once here, once re-entered inside
        // the worker by `blocking`.
        let entered = recorder.0.lock();
        let caller_entries = entered.iter().filter(|n| *n == "caller").count();
        assert_eq!(
            caller_entries, 2,
            "expected the caller span to be re-entered on the worker; saw {entered:?}"
        );
    }

    #[test]
    fn cpu_work_runs_on_the_rayon_pool() {
        let rt = get().expect("runtime");
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        // `install` blocks until the closure completes, so this is a
        // deterministic check that the pool executes work.
        rt.cpu().install(move || flag.store(true, Ordering::SeqCst));
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn the_blocking_marker_is_set_only_inside_runtime_blocking() {
        let rt = get().expect("runtime");
        assert!(!in_blocking_context(), "marker leaked onto the test thread");
        let inside = rt
            .rt
            .block_on(rt.blocking(in_blocking_context))
            .expect("join");
        assert!(inside, "marker not set inside runtime.blocking");
        assert!(!in_blocking_context(), "marker not restored after the call");
    }

    /// `shutdown` is what runs on quit, so a defect here is a hung or
    /// half-torn-down exit. It had no coverage at all.
    ///
    /// Uses its own runtime rather than the process-wide one: shutting the
    /// shared runtime down would break every other test.
    #[test]
    fn shutdown_drains_is_idempotent_and_refuses_later_work() {
        let rt = BackendRuntime::build().expect("build");

        assert!(!rt.is_shutting_down());
        assert!(
            rt.spawn(async {}).is_some(),
            "spawn refused before shutdown"
        );

        assert!(rt.shutdown_blocking(), "clean drain should succeed");
        assert!(rt.is_shutting_down());
        assert!(rt.shutdown_token().is_cancelled());

        // Idempotent.
        rt.shutdown_blocking();

        // And work submitted after shutdown is refused rather than queued onto
        // a tracker nobody will wait on.
        assert!(
            rt.spawn(async {}).is_none(),
            "spawn accepted after shutdown"
        );

        // Leak deliberately: dropping a Runtime here is the hazard the module
        // header describes, and the test process is about to end anyway.
        // `ManuallyDrop` rather than `mem::forget` so the intent is in the
        // type (and so it passes the crate's `clippy::mem_forget` deny).
        let _leaked = std::mem::ManuallyDrop::new(rt);
    }

    #[test]
    fn the_shutdown_grace_fits_inside_gpuis_budget() {
        // gpui enforces `App::SHUTDOWN_TIMEOUT` (200 ms) around quit futures.
        // Overrunning it freezes the window and can get the process killed.
        assert!(
            SHUTDOWN_GRACE < Duration::from_millis(200),
            "SHUTDOWN_GRACE ({SHUTDOWN_GRACE:?}) must stay under gpui's 200ms quit budget"
        );
    }

    #[test]
    fn cpu_threads_are_clamped_to_a_sane_range() {
        let n = cpu_threads();
        assert!((1..=8).contains(&n), "unexpected cpu thread count: {n}");
    }

    /// Pins the constraint that governs where `GitBackend::fetch` may run.
    ///
    /// `gix` fetches over `blocking-http-transport-reqwest-native-tls`, and
    /// `reqwest::blocking` refuses to run when it can see an ambient Tokio
    /// runtime — it would have to start a nested one. A `spawn_blocking`
    /// worker *does* see one (`Handle::current()` resolves there), so fetch
    /// must not be routed through [`BackendRuntime::blocking`]; it needs a
    /// plain `std::thread`, which has no ambient handle.
    ///
    /// If this ever starts failing, the constraint has changed and fetch can
    /// be simplified — but until then, moving fetch onto the blocking pool
    /// would panic at runtime, only on the network path, only for users with
    /// a remote configured.
    #[test]
    fn blocking_workers_see_an_ambient_runtime_but_plain_threads_do_not() {
        let rt = get().expect("runtime");

        let in_blocking = rt
            .rt
            .block_on(rt.blocking(|| Handle::try_current().is_ok()))
            .expect("join");
        assert!(
            in_blocking,
            "a spawn_blocking worker unexpectedly has no ambient runtime"
        );

        let in_plain_thread = std::thread::spawn(|| Handle::try_current().is_ok())
            .join()
            .expect("join");
        assert!(
            !in_plain_thread,
            "a plain thread unexpectedly has an ambient runtime; \
             routing gix fetch there would no longer be safe"
        );
    }
}
