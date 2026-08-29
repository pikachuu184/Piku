//! The one place a sprite-atlas tile is freed.
//!
//! # Why this exists
//!
//! A painted `Arc<RenderImage>` owns a tile in gpui's sprite atlas, and only
//! `Window::drop_image` frees it. Dropping the `Arc` alone leaks GPU memory for
//! the life of the process, so every cache and every view that discards a
//! decoded frame has to hand it over for release.
//!
//! Doing that *promptly* crashes the renderer. `WgpuAtlas::remove` deallocates
//! the tile and decrements its texture's reference count, and when that count
//! reaches zero it leaves a `None` in the texture list and pushes the slot onto a
//! free list. `get_texture_info` indexes that same list while recording draw
//! calls and `expect("texture must exist")`s on the hole. Note the granularity:
//! the hole is per *texture*, not per tile, so it takes out every tile that
//! shared the texture — and a video frame wider than half of gpui's 1024² atlas
//! is the only tile on its own texture, which makes every single release a
//! candidate.
//!
//! A recorded scene outlives the paint that recorded it in two ways, so the hole
//! is reachable:
//!
//! * **`present` without `draw`.** gpui re-submits the existing
//!   `rendered_frame.scene` with no new draw whenever presentation is required
//!   or input has recently arrived at 60 Hz or more — so during ordinary mouse
//!   movement the same scene is presented many times.
//! * **Cached-view replay.** `gpui-component` wraps every dock panel in
//!   `.cached(...)`, and a cached view that is not marked dirty has its
//!   primitives — atlas tile ids included — replayed into each new frame. A
//!   panel that does not re-render therefore keeps its tile references alive
//!   indefinitely, and updating a *model* it reads does not mark it dirty: only
//!   an explicit `cx.notify()` on the view itself does.
//!
//! So the rule callers used to follow — "release is safe, `paint_image`
//! re-interns on a miss, so the worst case is one re-upload" — is true of a
//! *future* paint and false of an *already recorded* scene. The real rule, and
//! the invariant this module implements, is:
//!
//! > Free a tile only once no other owner can still paint the image, and only
//! > after a draw that replays nothing has completed since that became true.
//!
//! # How
//!
//! [`release_later`] parks the image here. The reaper holds the last clone, so
//! `Arc::strong_count == 1` is proof that no owner is left to paint it; anything
//! still shared stays parked and is re-examined next cycle, which is exactly the
//! case the old code got wrong. A pump task ([`init`]) then drives a two-hop
//! frame chain per window, because `Window::on_next_frame` callbacks run at the
//! *top* of a frame, before that frame's draw:
//!
//! ```text
//! hop 1 (top of frame N)   window.refresh()  =>  frame N's draw replays nothing
//! frame N draws & presents
//! hop 2 (top of frame N+1) window.drop_image(..)
//! ```
//!
//! At hop 2 the presentable scene is frame N's, which cannot name the tile: it
//! replayed nothing, and no owner existed to paint it. Freeing is then safe
//! against both hazards above.
//!
//! Release is per-window (`Window::drop_image`, not `App::drop_image`) so each
//! window's tile is freed after *that* window's replay-free draw. Handing a batch
//! to a window that never painted it is free — `WgpuAtlas::remove` returns early
//! on an unknown key. It also side-steps a trap: `App::drop_image(image, None)`
//! frees nothing in the window currently being updated, because gpui *takes* that
//! window out of its map for the duration of the update.
//!
//! One caveat, and it is a deferral rather than a leak: the X11 frame loop is a
//! timer that stops while the window is hidden, so a batch armed just before the
//! window is minimized waits in `next_frame_callbacks` until it is shown again.

use std::sync::Arc;
use std::time::Duration;

use gpui::{App, Global, RenderImage, Window};

/// Delay between pump cycles while there is work to do.
///
/// Long enough that a 30 fps video's retired frames leave in one batch — one
/// full-window re-render per batch, not per frame — and short enough that the
/// pixel buffers behind them are not held long. Video queues roughly five
/// frames per cycle.
const BUSY_INTERVAL: Duration = Duration::from_millis(150);

/// Delay between pump cycles when nothing is reapable.
///
/// Reached by doubling from [`BUSY_INTERVAL`]. An image whose owner holds it for
/// minutes (an inspector preview, say) must not cost a 150 ms wakeup for all of
/// them, and neither should an idle app.
const IDLE_INTERVAL: Duration = Duration::from_secs(2);

/// Images awaiting release, and the pump's backoff.
///
/// A `Global` rather than a field of [`crate::state::PikuState`]: release
/// happens from `on_release` hooks that run during teardown, after any entity
/// this could live in may already be gone.
#[derive(Default)]
pub struct AtlasReaper {
    /// Parked images, deduped by [`gpui::ImageId`].
    pending: Vec<Arc<RenderImage>>,
    /// Current pump delay. `None` before the first cycle.
    interval: Option<Duration>,
}

impl Global for AtlasReaper {}

impl AtlasReaper {
    /// Park `image` for release. `false` if it was already parked.
    ///
    /// Deduplication is load-bearing, not tidiness: two clones parked here would
    /// leave the reaper itself holding two references and the strong-count gate
    /// in [`Self::take_unreferenced`] could never open, so the tile would never
    /// be freed.
    fn park(&mut self, image: Arc<RenderImage>) -> bool {
        if self.pending.iter().any(|held| held.id == image.id) {
            return false;
        }
        self.pending.push(image);
        // New work: go back to the short interval.
        self.interval = Some(BUSY_INTERVAL);
        true
    }

    /// Take every parked image that nothing else can still paint.
    ///
    /// The reaper holds one reference to each parked image, so a strong count of
    /// exactly one means every other owner has dropped it. Anything still shared
    /// stays parked.
    fn take_unreferenced(&mut self) -> Vec<Arc<RenderImage>> {
        let mut batch = Vec::new();
        let mut parked = Vec::with_capacity(self.pending.len());
        for image in self.pending.drain(..) {
            if Arc::strong_count(&image) == 1 {
                batch.push(image);
            } else {
                parked.push(image);
            }
        }
        self.pending = parked;
        batch
    }

    /// The delay before the next pump cycle, given whether this one found work.
    fn next_interval(&mut self, reaped: bool) -> Duration {
        let interval = if reaped || !self.pending.is_empty() {
            BUSY_INTERVAL
        } else {
            self.interval
                .map_or(BUSY_INTERVAL, |current| (current * 2).min(IDLE_INTERVAL))
        };
        self.interval = Some(interval);
        interval
    }
}

/// Hand `image` over for release.
///
/// Replaces every direct `drop_image` call: it is safe from anywhere, including
/// from inside a render pass and from an `on_release` hook during teardown, and
/// it will not free a tile a scene can still name.
pub fn release_later(image: Arc<RenderImage>, cx: &mut App) {
    cx.default_global::<AtlasReaper>().park(image);
}

/// Start the pump. Call once during bootstrap.
///
/// The pump lives on the foreground executor rather than being spawned on demand
/// by [`release_later`], which must stay callable during shutdown — when
/// spawning is a debug panic.
///
/// It never exits, which is safe for the same reason `VideoView::start_ticker`'s
/// loop is: a detached foreground task waiting on a timer is dropped when the
/// `App` goes away, not polled once more. `cx.update` after that point would
/// panic ("app was released before async operation completed"), so if this ever
/// starts firing at shutdown the loop needs an exit, not a `catch`.
pub fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            let delay = cx.update(cycle);
            cx.background_executor().timer(delay).await;
        }
    })
    .detach();
}

/// One pump cycle: arm a release pass if anything is ready, and report the
/// delay before the next cycle.
fn cycle(cx: &mut App) -> Duration {
    let windows = cx.windows();
    // With no window there is no frame chain to hang the release off, and no
    // atlas to free from either. Leave everything parked.
    let batch = if windows.is_empty() {
        Vec::new()
    } else {
        cx.default_global::<AtlasReaper>().take_unreferenced()
    };

    for handle in windows {
        if batch.is_empty() {
            break;
        }
        let batch = batch.clone();
        // A closed window is not a failure: its atlas went with it.
        let _ = handle.update(cx, |_, window, _| arm(batch, window));
    }

    let reaped = !batch.is_empty();
    let interval = cx.default_global::<AtlasReaper>().next_interval(reaped);
    if reaped {
        // The one signal that says the release path is alive: how many tiles went
        // and how many are still pinned by an owner. `piku=debug` shows it.
        tracing::debug!(
            reaped = batch.len(),
            parked = cx.default_global::<AtlasReaper>().pending.len(),
            "atlas reaper armed a release pass"
        );
    }
    interval
}

/// Schedule `batch` to be freed from `window` after one replay-free draw.
///
/// Two hops because `on_next_frame` runs *before* the frame's draw: the first
/// hop makes the next draw bypass cached-view reuse, the second runs once that
/// draw has been presented.
fn arm(batch: Vec<Arc<RenderImage>>, window: &mut Window) {
    window.on_next_frame(move |window, _| {
        // Suppresses subtree reuse for this frame only, so the scene about to be
        // recorded cannot replay a primitive that names one of these tiles.
        window.refresh();
        window.on_next_frame(move |window, _| {
            for image in batch {
                // `Err` only means the window had no atlas to remove from.
                let _ = window.drop_image(image);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::backend::services::preview::content::RawImage;
    use crate::preview::image_util::render_image_from_bgra;

    fn image() -> Arc<RenderImage> {
        render_image_from_bgra(RawImage {
            width: 1,
            height: 1,
            bgra: vec![0, 0, 0, 255],
        })
    }

    /// Parking the same image twice would leave the reaper holding two
    /// references, and the strong-count gate would never open again.
    #[test]
    fn parking_an_image_twice_keeps_one_reference() {
        let mut reaper = AtlasReaper::default();
        let image = image();
        assert!(reaper.park(image.clone()));
        assert!(
            !reaper.park(image.clone()),
            "the second park must be refused"
        );
        assert_eq!(reaper.pending.len(), 1);
        drop(image);
        assert_eq!(
            reaper.take_unreferenced().len(),
            1,
            "a doubly-parked image must still become reapable"
        );
    }

    /// The whole point: an image someone else can still paint is not freed.
    #[test]
    fn an_image_with_another_owner_is_not_reaped() {
        let mut reaper = AtlasReaper::default();
        let owner = image();
        assert!(reaper.park(owner.clone()));
        assert!(
            reaper.take_unreferenced().is_empty(),
            "a shared image must stay parked"
        );
        assert_eq!(reaper.pending.len(), 1);

        // The owner lets go: now nothing can paint it.
        drop(owner);
        let batch = reaper.take_unreferenced();
        assert_eq!(batch.len(), 1);
        assert!(reaper.pending.is_empty(), "a reaped image must not stay parked");
    }

    /// Every image handed over must come back out exactly once.
    #[test]
    fn distinct_images_are_all_reaped() {
        let mut reaper = AtlasReaper::default();
        for _ in 0..4 {
            assert!(reaper.park(image()));
        }
        assert_eq!(reaper.take_unreferenced().len(), 4);
        assert!(reaper.pending.is_empty());
        assert!(reaper.take_unreferenced().is_empty());
    }

    /// Idle cycles must back off, and new work must undo the backoff — the pump
    /// is a timer on the UI thread's executor.
    #[test]
    fn the_pump_backs_off_while_idle_and_speeds_up_on_new_work() {
        let mut reaper = AtlasReaper::default();
        let mut interval = reaper.next_interval(false);
        assert_eq!(interval, BUSY_INTERVAL);
        for _ in 0..8 {
            interval = reaper.next_interval(false);
        }
        assert_eq!(interval, IDLE_INTERVAL, "an idle pump must reach the floor");

        assert!(reaper.park(image()));
        assert_eq!(
            reaper.next_interval(true),
            BUSY_INTERVAL,
            "new work must undo the backoff"
        );
    }

    /// A batch still in flight must not stall the pump, but anything left parked
    /// must keep it on the short interval.
    #[test]
    fn parked_work_keeps_the_pump_busy() {
        let mut reaper = AtlasReaper::default();
        let owner = image();
        assert!(reaper.park(owner.clone()));
        assert!(reaper.take_unreferenced().is_empty());
        assert_eq!(reaper.next_interval(false), BUSY_INTERVAL);
        drop(owner);
    }
}
