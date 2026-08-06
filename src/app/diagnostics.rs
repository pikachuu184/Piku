//! Render-thread instrumentation: a span around each render pass, and a
//! debug-build assertion that nothing blocking happens inside one.
//!
//! The point of the backend re-architecture is that the render thread never
//! touches the filesystem. That is easy to claim and easy to regress, so it is
//! measured two ways here:
//!
//! * **`PIKU_TRACE_SPANS=1`** turns on span-close events, so `render` spans
//!   report their own duration and a run can be summarized into p50/p99.
//! * **[`assert_not_rendering`]** turns the invariant into a debug-build
//!   panic. Every remaining blocking entry point calls it; if any of them is
//!   ever reached from inside a render pass, the developer finds out
//!   immediately instead of shipping a stutter.
//!
//! Both are free in release builds: the guard compiles to nothing without
//! `debug_assertions`, and the span is a no-op when the subscriber filters it
//! out.

/// Whether span-close events (and therefore span timings) are enabled.
///
/// Read once — the environment does not change mid-process, and this is
/// consulted on the render path.
pub fn trace_spans_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("PIKU_TRACE_SPANS").is_some_and(|v| v != "0" && !v.is_empty())
    })
}

#[cfg(debug_assertions)]
mod guard {
    use std::cell::Cell;

    thread_local! {
        /// Set for the duration of a render pass on the thread running it.
        static RENDERING: Cell<bool> = const { Cell::new(false) };
    }

    /// Marks the current thread as rendering until dropped. Restores the
    /// previous value rather than clearing, so nesting is safe.
    pub struct RenderScope(bool);

    impl RenderScope {
        pub fn enter() -> Self {
            Self(RENDERING.with(|r| r.replace(true)))
        }
    }

    impl Drop for RenderScope {
        fn drop(&mut self) {
            RENDERING.with(|r| r.set(self.0));
        }
    }

    pub fn is_rendering() -> bool {
        RENDERING.with(Cell::get)
    }
}

#[cfg(not(debug_assertions))]
mod guard {
    pub struct RenderScope;
    impl RenderScope {
        #[inline(always)]
        pub fn enter() -> Self {
            Self
        }
    }
    #[inline(always)]
    pub fn is_rendering() -> bool {
        false
    }
}

pub use guard::RenderScope;

/// The thread gpui runs the event loop and rendering on, recorded at startup.
static UI_THREAD: std::sync::OnceLock<std::thread::ThreadId> = std::sync::OnceLock::new();

/// Record the calling thread as the UI thread. Called once from `app::run`
/// before any window exists.
pub fn mark_ui_thread() {
    let _ = UI_THREAD.set(std::thread::current().id());
}

/// Whether the caller is on the UI thread. False before [`mark_ui_thread`]
/// runs, and in tests, so neither trips the checks below.
fn on_ui_thread() -> bool {
    UI_THREAD
        .get()
        .is_some_and(|id| *id == std::thread::current().id())
}

/// Flag a blocking operation according to where it is running.
///
/// Call this from every operation that can block — filesystem syscalls, JSON
/// state writes, subprocess spawns. There are two severities, because they are
/// two different problems:
///
/// * **Inside a render pass** — a hard `debug_assert!`. Nothing may ever block
///   while the element tree is being built; that is a dropped frame every time.
/// * **On the UI thread but outside a render pass** — a `warn!` naming the
///   operation. This is the event-handler case (a folder navigation writing
///   `navigation.json`, a tab restore stat-ing 50 paths). It still stutters,
///   but the app remains usable, so it is reported rather than fatal. Each of
///   these becomes unreachable as its call site moves behind the backend, and
///   the warning is how you tell which ones are left.
///
/// Free in release builds and on background threads.
#[inline]
pub fn assert_not_rendering(what: &str) {
    if !cfg!(debug_assertions) {
        return;
    }
    if guard::is_rendering() {
        // `tracing` first: if a panic hook swallows the message, the log still
        // records which operation blocked.
        tracing::error!(operation = what, "blocking work inside a render pass");
        debug_assert!(
            false,
            "`{what}` ran inside a render pass — dispatch it to the backend instead"
        );
    } else if on_ui_thread() {
        tracing::warn!(
            target: "piku::render",
            operation = what,
            "blocking work on the UI thread",
        );
    }
}

/// Guards a render pass: holds the `render` span open and marks the thread as
/// rendering until dropped.
///
/// Held as a `let _guard` on the first line of a `Render::render` body, so it
/// covers the whole element-tree construction:
///
/// ```ignore
/// fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
///     let _guard = diagnostics::enter_render("ExplorerPanel");
///     // ... build the tree; any blocking call in here now panics in debug.
/// }
/// ```
#[must_use = "the guard must be held for the duration of the render pass"]
pub struct RenderPass {
    _span: tracing::span::EnteredSpan,
    _scope: RenderScope,
}

/// Open a render pass on the current thread. See [`RenderPass`].
#[inline]
pub fn enter_render(view: &'static str) -> RenderPass {
    RenderPass {
        _span: tracing::trace_span!(target: "piku::render", "render", view = view).entered(),
        _scope: RenderScope::enter(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assert_not_rendering_is_quiet_outside_a_render_pass() {
        // No panic: nothing is rendering on this thread.
        assert_not_rendering("fs::read_dir");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_guard_is_set_only_inside_a_render_pass() {
        assert!(!guard::is_rendering());
        {
            let _pass = enter_render("Test");
            assert!(guard::is_rendering());
        }
        assert!(!guard::is_rendering());
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_guard_restores_the_previous_value_when_nested() {
        let _outer = enter_render("Outer");
        {
            let _inner = enter_render("Inner");
            assert!(guard::is_rendering());
        }
        // Still rendering: the inner scope restored, not cleared.
        assert!(guard::is_rendering());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ran inside a render pass")]
    fn blocking_work_inside_a_render_pass_panics_in_debug_builds() {
        let _pass = enter_render("Test");
        assert_not_rendering("fs::metadata");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn the_guard_is_per_thread() {
        let _pass = enter_render("Test");
        // Another thread is not rendering just because this one is.
        let elsewhere = std::thread::spawn(guard::is_rendering).join().unwrap();
        assert!(!elsewhere);
    }

    #[test]
    fn the_ui_thread_is_unset_in_tests_so_nothing_warns() {
        // `mark_ui_thread` is only called from `app::run`, so the UI-thread
        // branch stays inert under `cargo test` and cannot make an unrelated
        // test noisy or flaky.
        assert!(!on_ui_thread());
    }
}
