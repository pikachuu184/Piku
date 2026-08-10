//! Resident thumbnails, and the demand that drives decoding them.
//!
//! This type used to own the work: a queue, a concurrency gate, and a
//! `cx.spawn` per item, fired from inside a render pass. It now owns only
//! storage and demand. Decoding lives in the preview service, which means it
//! is bounded, authorized, and — the point — **cancellable**.
//!
//! Two behaviours worth naming, because they are what the change bought:
//!
//! * **Order is priority.** [`request`](ThumbnailCache::request) records
//!   demand in first-seen order, and the file list is virtualized, so the
//!   renderer only asks for rows it is about to draw. First-seen order is
//!   therefore visible order, and the service decodes in exactly that order.
//! * **Scrolling supersedes.** Each flush drops the previous stream's
//!   `Inflight`, so a fast scroll stops the old batch instead of decoding a
//!   screenful nobody is looking at any more. Before, every queued decode ran
//!   to completion regardless.
//!
//! The debounce also moves dispatch *out* of the render pass: `request` now
//! pushes to a list and arms a timer, where it used to spawn a task per item
//! while an element tree was being built.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, RenderImage};

use crate::backend::dispatch::{BackendExt as _, Flow};
use crate::backend::protocol::{Inflight, StreamItem};
use crate::backend::services::preview::{ThumbKey, ThumbRequest, ThumbSource};
use crate::core::entry::FsEntry;
use crate::core::file_type::{FileCategory, categorize};
use crate::preview::image_util::render_image_from_bgra;

pub use crate::backend::services::preview::THUMB_TARGET;

/// Most decoded thumbnails kept resident; the least-recently-used is evicted
/// past this. At ~96×96×4 bytes each this caps resident memory near ~18 MiB.
const CACHE_CAP: usize = 512;

/// How long demand accumulates before a batch is issued.
///
/// Long enough that a fast scroll produces one dispatch rather than one per
/// frame, short enough to be invisible. It also guarantees the dispatch happens
/// after the render pass that asked for it has finished.
const FLUSH_DELAY: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct ThumbnailCache {
    ready: HashMap<ThumbKey, Arc<RenderImage>>,
    /// LRU order, front = oldest.
    lru: VecDeque<ThumbKey>,
    /// Permanent failures. Never retried at this key; a changed mtime makes a
    /// new key, so an edited file gets another chance.
    failed: HashSet<ThumbKey>,
    /// Handed to the service and not yet answered.
    inflight: HashSet<ThumbKey>,
    /// Demand accumulated since the last flush, in first-seen (visible) order.
    pending: Vec<(ThumbKey, ThumbSource)>,
    /// Armed timer, so demand from one render pass costs one dispatch.
    flush_armed: bool,
    /// The live batch. Dropping it cancels the decoder.
    batch: Option<Inflight>,
}

impl ThumbnailCache {
    /// The decoded thumbnail for this entry, if ready. Bumps its LRU position.
    pub fn get(&mut self, entry: &FsEntry, target: u32) -> Option<Arc<RenderImage>> {
        let key = ThumbKey::for_entry(entry, target);
        let img = self.ready.get(&key).cloned()?;
        self.touch(&key);
        Some(img)
    }

    /// Ensure a thumbnail for this entry is decoding or ready. Cheap and
    /// idempotent — safe to call on every render for every visible entry, which
    /// is exactly how `entry_visual` uses it.
    pub fn request(&mut self, entry: &FsEntry, target: u32, cx: &mut Context<Self>) {
        let source = match categorize(entry) {
            FileCategory::Image => ThumbSource::Image,
            FileCategory::Video => ThumbSource::Video,
            _ => return,
        };
        let key = ThumbKey::for_entry(entry, target);
        if self.ready.contains_key(&key)
            || self.failed.contains(&key)
            || self.inflight.contains(&key)
            || self.pending.iter().any(|(k, _)| k == &key)
        {
            return;
        }
        self.pending.push((key, source));
        self.arm_flush(cx);
    }

    /// Forget everything known about `path`, so the next request re-decodes.
    ///
    /// For the watcher: a file edited in place keeps its path but should not
    /// keep a stale thumbnail — and, more subtly, should not keep a stale
    /// *failure*.
    #[allow(dead_code, reason = "wired to the watch service in Stage 6")]
    pub fn evict_path(&mut self, path: &std::path::Path) {
        self.ready.retain(|k, _| k.path != path);
        self.failed.retain(|k| k.path != path);
        self.lru.retain(|k| k.path != path);
        self.pending.retain(|(k, _)| k.path != path);
    }

    fn touch(&mut self, key: &ThumbKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
    }

    fn arm_flush(&mut self, cx: &mut Context<Self>) {
        if self.flush_armed {
            return;
        }
        self.flush_armed = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FLUSH_DELAY).await;
            let _ = this.update(cx, |this, cx| this.flush(cx));
        })
        .detach();
    }

    fn flush(&mut self, cx: &mut Context<Self>) {
        self.flush_armed = false;
        // Anything that landed while the timer ran is no longer wanted.
        let pending = std::mem::take(&mut self.pending);
        let mut batch: Vec<ThumbRequest> = Vec::with_capacity(pending.len());
        for (key, source) in pending {
            if self.ready.contains_key(&key) || self.failed.contains(&key) {
                continue;
            }
            self.inflight.insert(key.clone());
            batch.push(ThumbRequest { key, source });
        }
        if batch.is_empty() {
            self.batch = None;
            return;
        }

        // Assigning drops the previous Inflight, which stops the superseded
        // batch. Its keys are released when its terminal item arrives.
        self.batch = Some(cx.backend_stream(
            move |backend| backend.preview().thumbnails(batch),
            |this: &mut Self, item, cx| {
                match item {
                    StreamItem::Batch(thumbs) => {
                        for thumb in thumbs {
                            this.inflight.remove(&thumb.key);
                            match thumb.image {
                                Some(raw) => {
                                    this.ready
                                        .insert(thumb.key.clone(), render_image_from_bgra(raw));
                                    this.touch(&thumb.key);
                                }
                                None => {
                                    this.failed.insert(thumb.key);
                                }
                            }
                        }
                        this.evict();
                    }
                    StreamItem::Progress(_) => {}
                    StreamItem::Done(_) => {
                        // Whatever the batch did not answer was cancelled, not
                        // failed. Clearing the set is what lets those keys be
                        // offered again — without it they would be wedged as
                        // permanently in flight and their tiles would keep the
                        // glyph forever. `dispatch` guarantees this item
                        // arrives even if the producer dies.
                        this.inflight.clear();
                        if !this.pending.is_empty() {
                            this.arm_flush(cx);
                        }
                    }
                }
                Flow::Continue
            },
        ));
    }

    fn evict(&mut self) {
        while self.ready.len() > CACHE_CAP {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            self.ready.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key(name: &str) -> ThumbKey {
        ThumbKey {
            path: PathBuf::from(name),
            mtime: 1,
            target: THUMB_TARGET,
        }
    }

    fn blank() -> Arc<RenderImage> {
        render_image_from_bgra(crate::backend::services::preview::content::RawImage {
            width: 1,
            height: 1,
            bgra: vec![0, 0, 0, 255],
        })
    }

    /// Eviction is the cache's one real invariant: it must stay bounded.
    #[test]
    fn the_cache_evicts_the_least_recently_used_past_its_cap() {
        let mut cache = ThumbnailCache::default();
        for i in 0..CACHE_CAP + 10 {
            let k = key(&format!("f{i}.png"));
            cache.ready.insert(k.clone(), blank());
            cache.touch(&k);
            cache.evict();
        }
        assert_eq!(cache.ready.len(), CACHE_CAP);
        assert!(
            !cache.ready.contains_key(&key("f0.png")),
            "the oldest entry survived eviction"
        );
        assert!(cache.ready.contains_key(&key("f511.png")));
    }

    /// A hit must move the entry to the back of the LRU, or a thumbnail that is
    /// on screen the whole time gets evicted out from under the row showing it.
    #[test]
    fn a_hit_protects_an_entry_from_the_next_eviction() {
        let mut cache = ThumbnailCache::default();
        let hot = key("hot.png");
        cache.ready.insert(hot.clone(), blank());
        cache.touch(&hot);
        for i in 0..CACHE_CAP + 5 {
            let k = key(&format!("f{i}.png"));
            cache.ready.insert(k.clone(), blank());
            cache.touch(&k);
            // Keep it warm, the way rendering its row would.
            cache.touch(&hot);
            cache.evict();
        }
        assert!(cache.ready.contains_key(&hot), "a hot entry was evicted");
    }

    #[test]
    fn evicting_a_path_forgets_its_failure_too() {
        let mut cache = ThumbnailCache::default();
        let k = key("broken.png");
        cache.failed.insert(k.clone());
        cache.ready.insert(k.clone(), blank());
        cache.lru.push_back(k.clone());
        cache.evict_path(&k.path);
        assert!(
            !cache.failed.contains(&k),
            "an edited file must get another chance to decode"
        );
        assert!(!cache.ready.contains_key(&k));
        assert!(!cache.lru.contains(&k));
    }
}
