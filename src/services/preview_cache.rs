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

use gpui::{App, Entity, RenderImage};

use crate::preview::content::{PreviewContent, PreviewImage};
use crate::services::atlas_reaper;

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
    ///
    /// Returns every `RenderImage` that left the cache — replaced or evicted —
    /// because each one owns a sprite-atlas tile that nothing but the reaper
    /// frees. Dropping the `Arc` alone leaks GPU memory for the life of the
    /// process. Returning them rather than taking a `&mut App` keeps this type
    /// testable without a gpui harness.
    #[must_use = "the returned images still own GPU memory; pass them to services::atlas_reaper::release_later"]
    pub fn insert(
        &mut self,
        key: PreviewKey,
        content: Arc<PreviewContent>,
    ) -> Vec<Arc<RenderImage>> {
        if !cacheable(&content) {
            return Vec::new();
        }
        let mut dropped = Vec::new();
        if let Some(old) = self.ready.insert(key.clone(), content.clone()) {
            // Replacing an existing entry: drop its byte estimate first.
            self.bytes = self.bytes.saturating_sub(estimated_bytes(&old));
            if let Some(pos) = self.lru.iter().position(|k| k == &key) {
                self.lru.remove(pos);
            }
            // Re-inserting the *same* payload (both panels cache one decode) is
            // not a discard: the entry that "left" is the one still held, so
            // listing it would park a live image for no reason.
            if !Arc::ptr_eq(&old, &content) {
                content_images(&old, &mut dropped);
            }
        }
        self.bytes = self.bytes.saturating_add(estimated_bytes(&content));
        self.lru.push_back(key);
        self.evict(&mut dropped);
        dropped
    }

    fn touch(&mut self, key: &PreviewKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
    }

    fn evict(&mut self, dropped: &mut Vec<Arc<RenderImage>>) {
        while self.ready.len() > CACHE_CAP || self.bytes > BYTE_BUDGET {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            if let Some(content) = self.ready.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(estimated_bytes(&content));
                content_images(&content, dropped);
            }
        }
    }
}

/// Every decoded frame inside `content`, appended to `out`.
///
/// Deliberately shaped like [`estimated_bytes`] and kept next to it: a variant
/// that holds an image and is billed by one but missed by the other is exactly
/// how a leak comes back. Listing an image another owner still holds is safe
/// *because* release goes through
/// [`atlas_reaper`](crate::services::atlas_reaper), which will not free a tile
/// until nothing can paint it — this must not be read as licence to free one
/// eagerly.
fn content_images(content: &PreviewContent, out: &mut Vec<Arc<RenderImage>>) {
    match content {
        PreviewContent::Image {
            source: PreviewImage::Decoded(frame),
            ..
        } => out.push(frame.clone()),
        PreviewContent::Video {
            poster: Some(poster),
            ..
        } => out.push(poster.clone()),
        PreviewContent::Pdf { pages, .. } => out.extend(pages.iter().cloned()),
        // SVG rasterizes inside the renderer and everything else is text or
        // metadata: no buffer of ours, so no tile of ours.
        _ => {}
    }
}

/// Cache `content` under `key`, handing whatever that displaced to the reaper.
///
/// The inspector and the media panel both decode-then-cache on the same shape of
/// callback, and both have to release what [`PreviewCache::insert`] hands back.
/// That release lives here, once, rather than being copied to each call site and
/// eventually forgotten at one of them.
pub fn cache_preview(
    cache: &Entity<PreviewCache>,
    key: PreviewKey,
    content: Arc<PreviewContent>,
    cx: &mut App,
) {
    for image in cache.update(cx, |c, _| c.insert(key, content)) {
        // Not `drop_image`: eviction is blind to what is on screen. The panel
        // still displaying an evicted preview holds its own `Arc` and is not
        // marked dirty by a cache update, so its cached subtree keeps replaying
        // the tile. The reaper waits until that `Arc` is gone.
        atlas_reaper::release_later(image, cx);
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
fn render_image_bytes(frame: &Arc<RenderImage>) -> usize {
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
        let mut released = 0usize;
        for i in 0..CACHE_CAP {
            released += cache
                .insert(key(&format!("f{i}.png"), 1), decoded_image(edge))
                .len();
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
        // The leak this closed: every evicted frame must come back so its atlas
        // tile can be freed, or 16 MiB of VRAM goes with each one.
        assert_eq!(
            released,
            CACHE_CAP - cache.ready.len(),
            "every evicted image must be handed back for release"
        );
    }

    /// Overwriting a key is a discard too — distinct from eviction, and the
    /// easier of the two to miss.
    #[test]
    fn replacing_an_entry_hands_back_the_old_image() {
        let mut cache = PreviewCache::default();
        let k = key("frame.png", 1);
        assert!(cache.insert(k.clone(), decoded_image(16)).is_empty());
        let released = cache.insert(k.clone(), decoded_image(16));
        assert_eq!(
            released.len(),
            1,
            "the replaced image must be handed back for release"
        );
    }

    /// Both panels cache the same decode under the same key. Treating that as a
    /// discard would release a tile for an image still on screen.
    #[test]
    fn reinserting_the_same_payload_releases_nothing() {
        let mut cache = PreviewCache::default();
        let k = key("frame.png", 1);
        let content = decoded_image(16);
        assert!(cache.insert(k.clone(), content.clone()).is_empty());
        assert!(
            cache.insert(k, content).is_empty(),
            "re-caching one payload must not release its own tiles"
        );
    }

    /// The walker must stay in step with [`estimated_bytes`]: a variant billed
    /// for a frame but not searched for one leaks it.
    #[test]
    fn the_image_walker_finds_every_billed_frame() {
        let frame = |edge: u32| {
            crate::preview::image_util::render_image_from_bgra(
                crate::backend::services::preview::content::RawImage {
                    width: edge,
                    height: edge,
                    bgra: vec![0u8; (edge as usize) * (edge as usize) * 4],
                },
            )
        };
        let cases: Vec<(&str, PreviewContent, usize)> = vec![
            (
                "video poster",
                PreviewContent::Video {
                    rows: Vec::new(),
                    poster: Some(frame(64)),
                },
                1,
            ),
            (
                "pdf pages",
                PreviewContent::Pdf {
                    pages: vec![frame(64), frame(64), frame(64)],
                    total_pages: 3,
                    note: None,
                },
                3,
            ),
            (
                "decoded image",
                PreviewContent::Image {
                    source: PreviewImage::Decoded(frame(64)),
                    dimensions: Some((64, 64)),
                },
                1,
            ),
            (
                "video without a poster",
                PreviewContent::Video {
                    rows: Vec::new(),
                    poster: None,
                },
                0,
            ),
            (
                "svg",
                PreviewContent::Image {
                    source: PreviewImage::Path(std::path::PathBuf::from("logo.svg")),
                    dimensions: None,
                },
                0,
            ),
        ];
        for (name, content, expected) in cases {
            let mut found = Vec::new();
            content_images(&content, &mut found);
            assert_eq!(found.len(), expected, "wrong frame count for {name}");
            // The two must agree: anything billed for a buffer holds a tile.
            let billed = estimated_bytes(&content) > SMALL_PAYLOAD_EST;
            assert_eq!(
                billed,
                expected > 0,
                "{name} is billed as {} but walked as {}",
                if billed { "a buffer" } else { "metadata" },
                if expected > 0 { "a buffer" } else { "metadata" }
            );
        }
    }

    #[test]
    fn get_returns_inserted() {
        let mut cache = PreviewCache::default();
        let k = key("a.mp3", 1);
        drop(cache.insert(k.clone(), audio(10)));
        assert!(cache.get(&k).is_some());
    }

    #[test]
    fn changed_mtime_is_a_miss() {
        let mut cache = PreviewCache::default();
        drop(cache.insert(key("a.mp3", 1), audio(10)));
        // Same path, newer mtime → different key → not found (forces re-decode).
        assert!(cache.get(&key("a.mp3", 2)).is_none());
    }

    #[test]
    fn count_eviction_drops_oldest() {
        let mut cache = PreviewCache::default();
        for i in 0..(CACHE_CAP + 5) {
            drop(cache.insert(key(&format!("f{i}.mp3"), 1), audio(1)));
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
        drop(cache.insert(k.clone(), Arc::new(PreviewContent::Error("nope".into()))));
        assert!(cache.get(&k).is_none());
    }
}
