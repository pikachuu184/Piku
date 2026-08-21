//! A small, memory-bounded cache of decoded [`PreviewContent`], shared by the
//! inspector and the media panel so re-selecting or re-opening a file it has
//! already seen is instant instead of re-running the whole decode.
//!
//! Why this exists: building a preview is expensive for exactly the files users
//! revisit most — audio re-decodes the entire track to rebuild the waveform,
//! and video re-spawns an ffmpeg subprocess to extract a poster. Neither result
//! was cached across selections, so navigating away and back paid the full cost
//! again. This cache keys on `(path, mtime, size)` — a changed file re-decodes,
//! never served stale — and evicts on both a count cap and a total-byte budget
//! (LRU), so it can never grow without bound.
//!
//! The cache only *stores* results; the callers still own decoding on the
//! background executor. That keeps this type trivially correct: it is a plain
//! map with no I/O and no threads.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::preview::content::{PreviewContent, PreviewImage};

/// The key is defined by the service, not here, and the service echoes back the
/// one it actually read under. That is what lets the media panel probe the cache
/// without stat-ing on the UI thread the way it used to.
pub use crate::backend::services::preview::PreviewKey;

/// Most decoded previews kept resident regardless of size.
const CACHE_CAP: usize = 24;

/// Total estimated bytes kept resident. Small payloads (text/waveforms) never
/// approach this; it exists to bound image-heavy content (PDF pages, video
/// posters). The least-recently-used entries are evicted until both this and
/// [`CACHE_CAP`] hold.
const BYTE_BUDGET: usize = 96 * 1024 * 1024;

/// Floor charged to any resident entry, so a cache full of tiny payloads still
/// accounts for its own bookkeeping (the key's path, the LRU slot) rather than
/// appearing free.
const SMALL_PAYLOAD_EST: usize = 4 * 1024;

#[derive(Default)]
pub struct PreviewCache {
    ready: HashMap<PreviewKey, Arc<PreviewContent>>,
    /// LRU order, front = oldest. Bytes tracked alongside for the budget.
    lru: VecDeque<PreviewKey>,
    bytes: usize,
}

impl PreviewCache {
    /// The cached preview for this key, if present. Bumps its LRU position.
    pub fn get(&mut self, key: &PreviewKey) -> Option<Arc<PreviewContent>> {
        let content = self.ready.get(key).cloned()?;
        self.touch(key);
        Some(content)
    }

    /// Store a decoded preview. No-op for payloads we deliberately don't cache
    /// (see [`cacheable`]); otherwise inserts and evicts back under budget.
    pub fn insert(&mut self, key: PreviewKey, content: Arc<PreviewContent>) {
        if !cacheable(&content) {
            return;
        }
        if let Some(old) = self.ready.insert(key.clone(), content.clone()) {
            // Replacing an existing entry: drop its byte estimate first.
            self.bytes = self.bytes.saturating_sub(estimated_bytes(&old));
            if let Some(pos) = self.lru.iter().position(|k| k == &key) {
                self.lru.remove(pos);
            }
        }
        self.bytes = self.bytes.saturating_add(estimated_bytes(&content));
        self.lru.push_back(key);
        self.evict();
    }

    fn touch(&mut self, key: &PreviewKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
    }

    fn evict(&mut self) {
        while self.ready.len() > CACHE_CAP || self.bytes > BYTE_BUDGET {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(content) = self.ready.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(estimated_bytes(&content));
            }
        }
    }
}

/// Whether a payload is worth caching. Everything cheap to hold is; the only
/// exclusions are error/too-large sentinels (trivial to recompute and pointless
/// to pin) — image-heavy content is allowed but bounded by [`BYTE_BUDGET`].
fn cacheable(content: &PreviewContent) -> bool {
    !matches!(
        content,
        PreviewContent::Error(_) | PreviewContent::TooLarge { .. }
    )
}

/// A conservative resident-size estimate, used only for eviction accounting.
fn estimated_bytes(content: &PreviewContent) -> usize {
    match content {
        // An image preview is a *decoded* frame, not a path. It stopped being a
        // path when the decode moved to the worker so the grid and the
        // inspector could share one buffer with one set of limits — but this
        // arm kept billing it as metadata, at 4 KiB against a 96 MiB budget.
        // That left `CACHE_CAP` as the only real bound on image memory, which
        // at a 2048-px edge is roughly 384 MiB: four times the budget this
        // module exists to enforce. Bill the buffer.
        PreviewContent::Image { source, .. } => image_bytes(source),
        PreviewContent::Video {
            poster: Some(poster),
            ..
        } => render_image_bytes(poster),
        PreviewContent::Pdf { pages, .. } => pages.iter().map(render_image_bytes).sum(),
        PreviewContent::Code { text, .. }
        | PreviewContent::Markdown { source: text, .. }
        | PreviewContent::Structured { text, .. } => text.len(),
        PreviewContent::Audio { waveform, .. } => waveform.len() * 4,
        // Everything else is small metadata.
        _ => SMALL_PAYLOAD_EST,
    }
}

/// Resident bytes for a preview image.
fn image_bytes(source: &PreviewImage) -> usize {
    match source {
        // SVG really is just a path — the renderer rasterizes it itself, and
        // whatever it caches is the renderer's own accounting, not ours.
        PreviewImage::Path(_) => SMALL_PAYLOAD_EST,
        PreviewImage::Decoded(frame) => render_image_bytes(frame),
    }
}

/// Resident bytes for a decoded frame: four channels at one byte each.
///
/// `size()` is public API, so this does not depend on `RenderImage` internals —
/// which is what the flat constant was avoiding, at the cost of being wrong.
fn render_image_bytes(frame: &Arc<gpui::RenderImage>) -> usize {
    let size = frame.size(0);
    let width = u32::from(size.width) as usize;
    let height = u32::from(size.height) as usize;
    width
        .saturating_mul(height)
        .saturating_mul(4)
        .max(SMALL_PAYLOAD_EST)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(peaks: usize) -> Arc<PreviewContent> {
        Arc::new(PreviewContent::Audio {
            rows: Vec::new(),
            waveform: vec![0.5; peaks],
            duration_ms: 1000,
        })
    }

    fn key(name: &str, mtime: u64) -> PreviewKey {
        // Built through the constructor rather than a struct literal: the key's
        // fields belong to the service, so only it decides what identity means.
        PreviewKey::new(
            std::path::PathBuf::from(name),
            Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime)),
            1,
        )
    }

    /// A decoded image preview of `edge × edge` pixels.
    fn decoded_image(edge: u32) -> Arc<PreviewContent> {
        let raw = crate::backend::services::preview::content::RawImage {
            width: edge,
            height: edge,
            bgra: vec![0u8; (edge as usize) * (edge as usize) * 4],
        };
        Arc::new(PreviewContent::Image {
            source: PreviewImage::Decoded(crate::preview::image_util::render_image_from_bgra(raw)),
            dimensions: Some((edge, edge)),
        })
    }

    /// The defect this replaced: a decoded frame was billed as metadata, so
    /// `CACHE_CAP` was the only bound on image memory and the byte budget —
    /// the thing this module exists for — never fired for the payload that
    /// actually needs it.
    #[test]
    fn a_decoded_image_is_billed_its_real_buffer_size() {
        let edge = 256;
        let expected = (edge as usize) * (edge as usize) * 4;
        assert_eq!(estimated_bytes(&decoded_image(edge)), expected);
    }

    /// SVG is the one image that really is just a path: the renderer rasterizes
    /// it, so there is no buffer of ours to bill.
    #[test]
    fn an_svg_preview_is_billed_as_metadata() {
        let content = Arc::new(PreviewContent::Image {
            source: PreviewImage::Path(std::path::PathBuf::from("logo.svg")),
            dimensions: None,
        });
        assert_eq!(estimated_bytes(&content), SMALL_PAYLOAD_EST);
    }

    /// The budget must actually bind on images, not just the count cap. At a
    /// 2048-px edge one frame is 16 MiB, so a 96 MiB budget holds six — far
    /// fewer than `CACHE_CAP`, which is the whole point.
    #[test]
    fn the_byte_budget_evicts_images_before_the_count_cap_does() {
        let mut cache = PreviewCache::default();
        let edge = 2048;
        let per_image = (edge as usize) * (edge as usize) * 4;
        for i in 0..CACHE_CAP {
            cache.insert(key(&format!("f{i}.png"), 1), decoded_image(edge));
        }
        assert!(
            cache.ready.len() < CACHE_CAP,
            "the byte budget must evict before the count cap is reached"
        );
        assert!(
            cache.bytes <= BYTE_BUDGET,
            "resident bytes {} exceed the budget {BYTE_BUDGET}",
            cache.bytes
        );
        assert!(
            cache.ready.len() <= BYTE_BUDGET / per_image + 1,
            "resident count {} implies more memory than the budget allows",
            cache.ready.len()
        );
    }

    #[test]
    fn get_returns_inserted() {
        let mut cache = PreviewCache::default();
        let k = key("a.mp3", 1);
        cache.insert(k.clone(), audio(10));
        assert!(cache.get(&k).is_some());
    }

    #[test]
    fn changed_mtime_is_a_miss() {
        let mut cache = PreviewCache::default();
        cache.insert(key("a.mp3", 1), audio(10));
        // Same path, newer mtime → different key → not found (forces re-decode).
        assert!(cache.get(&key("a.mp3", 2)).is_none());
    }

    #[test]
    fn count_eviction_drops_oldest() {
        let mut cache = PreviewCache::default();
        for i in 0..(CACHE_CAP + 5) {
            cache.insert(key(&format!("f{i}.mp3"), 1), audio(1));
        }
        assert!(cache.ready.len() <= CACHE_CAP);
        // The very first inserts should have been evicted.
        assert!(cache.get(&key("f0.mp3", 1)).is_none());
        // The most recent should survive.
        assert!(
            cache
                .get(&key(&format!("f{}.mp3", CACHE_CAP + 4), 1))
                .is_some()
        );
    }

    #[test]
    fn errors_are_not_cached() {
        let mut cache = PreviewCache::default();
        let k = key("bad", 1);
        cache.insert(k.clone(), Arc::new(PreviewContent::Error("nope".into())));
        assert!(cache.get(&k).is_none());
    }
}
