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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::preview::content::PreviewContent;

/// Most decoded previews kept resident regardless of size.
const CACHE_CAP: usize = 24;

/// Total estimated bytes kept resident. Small payloads (text/waveforms) never
/// approach this; it exists to bound image-heavy content (PDF pages, video
/// posters). The least-recently-used entries are evicted until both this and
/// [`CACHE_CAP`] hold.
const BYTE_BUDGET: usize = 96 * 1024 * 1024;

/// Flat per-image estimate used only for eviction accounting. Real decoded
/// images vary, but a conservative constant keeps the cache honestly bounded
/// without depending on any RenderImage internals.
const IMAGE_BYTES_EST: usize = 8 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PreviewKey {
    path: PathBuf,
    /// Whole seconds since the epoch, `0` when unknown — an edited file gets a
    /// new key and is re-decoded rather than served from a stale entry.
    mtime: u64,
    size: u64,
}

impl PreviewKey {
    /// Build a key from an already-known modification time and size (the
    /// inspector path, which has an `FsEntry`).
    pub fn new(path: PathBuf, modified: Option<std::time::SystemTime>, size: u64) -> Self {
        let mtime = modified
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { path, mtime, size }
    }

    /// Build a key by stat-ing the file (the media-panel path, which only has a
    /// bare path). Returns `None` when the file cannot be stat-ed, so the caller
    /// simply decodes without caching rather than keying on bad metadata.
    pub fn for_path(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self::new(path.to_path_buf(), meta.modified().ok(), meta.len()))
    }
}

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
    !matches!(content, PreviewContent::Error(_) | PreviewContent::TooLarge { .. })
}

/// A conservative resident-size estimate, used only for eviction accounting.
fn estimated_bytes(content: &PreviewContent) -> usize {
    match content {
        PreviewContent::Video { poster: Some(_), .. } => IMAGE_BYTES_EST,
        PreviewContent::Pdf { pages, .. } => pages.len() * IMAGE_BYTES_EST,
        PreviewContent::Code { text, .. }
        | PreviewContent::Markdown { source: text, .. }
        | PreviewContent::Structured { text, .. } => text.len(),
        PreviewContent::Audio { waveform, .. } => waveform.len() * 4,
        // Image content is just a path + dimensions (gpui decodes lazily);
        // everything else is small metadata.
        _ => 4 * 1024,
    }
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
        PreviewKey { path: PathBuf::from(name), mtime, size: 1 }
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
        assert!(cache.get(&key(&format!("f{}.mp3", CACHE_CAP + 4), 1)).is_some());
    }

    #[test]
    fn errors_are_not_cached() {
        let mut cache = PreviewCache::default();
        let k = key("bad", 1);
        cache.insert(k.clone(), Arc::new(PreviewContent::Error("nope".into())));
        assert!(cache.get(&k).is_none());
    }
}
