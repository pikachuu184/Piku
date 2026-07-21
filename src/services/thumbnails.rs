//! Off-thread image thumbnail decoding with a bounded, self-draining work
//! queue and an LRU cache of BGRA [`RenderImage`]s for the file grid/list.
//!
//! The grid renders every entry (it is not virtualized), so the cache — not
//! the caller — bounds the work: requests are deduplicated and only
//! [`MAX_ACTIVE`] decodes run at once, the rest draining as each completes.
//! Every decode runs on the background executor; the UI thread only ever
//! looks up already-decoded results.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufReader, Seek as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use gpui::{Context, RenderImage};

use crate::core::entry::FsEntry;
use crate::core::file_type::{FileCategory, categorize};
use crate::preview::image_util::render_image_from_rgba;

/// Which decoder a queued thumbnail needs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThumbSource {
    Image,
    Video,
}

/// Longest edge (px) of a generated thumbnail. One size serves both the grid
/// (~34px) and the list (~16px) even at high DPI, so a file is decoded once
/// and both views share the result.
pub const THUMB_TARGET: u32 = 96;

/// Files larger than this keep the glyph icon — bounds worst-case decode work
/// for hostile or enormous images.
const THUMB_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Decompression-bomb guard: refuse images whose header promises more pixels
/// than this before any pixel buffer is allocated.
const THUMB_MAX_PIXELS: u64 = 100_000_000;

/// Most decoded thumbnails kept resident; the least-recently-used is evicted
/// past this. At ~96×96×4 bytes each this caps resident memory near ~18 MiB.
const CACHE_CAP: usize = 512;

/// Concurrent background decodes.
const MAX_ACTIVE: usize = 6;

#[derive(Clone, PartialEq, Eq, Hash)]
struct ThumbKey {
    path: PathBuf,
    /// Modification time (whole seconds since the epoch) so an edited file is
    /// re-decoded rather than served stale.
    mtime: u64,
    target: u32,
}

impl ThumbKey {
    fn for_entry(entry: &FsEntry, target: u32) -> Self {
        let mtime = entry
            .modified
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self { path: entry.path.clone(), mtime, target }
    }
}

#[derive(Default)]
pub struct ThumbnailCache {
    ready: HashMap<ThumbKey, Arc<RenderImage>>,
    lru: VecDeque<ThumbKey>,
    failed: HashSet<ThumbKey>,
    inflight: HashSet<ThumbKey>,
    queue: VecDeque<(ThumbKey, ThumbSource)>,
    active: usize,
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
    /// idempotent — safe to call on every render for every visible entry.
    /// Images decode directly; videos extract a poster frame via ffmpeg (which
    /// silently fails to a glyph when ffmpeg is not bundled/available).
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
            || self.queue.iter().any(|(k, _)| k == &key)
        {
            return;
        }
        self.queue.push_back((key, source));
        self.pump(cx);
    }

    fn touch(&mut self, key: &ThumbKey) {
        if let Some(pos) = self.lru.iter().position(|k| k == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.clone());
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.active < MAX_ACTIVE {
            let Some((key, source)) = self.queue.pop_front() else {
                break;
            };
            self.inflight.insert(key.clone());
            self.active += 1;
            let path = key.path.clone();
            let target = key.target;
            cx.spawn(async move |this, cx| {
                let decoded = cx
                    .background_executor()
                    .spawn(async move {
                        match source {
                            ThumbSource::Image => decode_thumbnail(&path, target),
                            ThumbSource::Video => {
                                crate::services::video_probe::poster_frame(&path, Some(target))
                            }
                        }
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.finish(key, decoded);
                    this.pump(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn finish(&mut self, key: ThumbKey, decoded: Option<Arc<RenderImage>>) {
        self.inflight.remove(&key);
        self.active = self.active.saturating_sub(1);
        match decoded {
            Some(img) => {
                self.ready.insert(key.clone(), img);
                self.touch(&key);
                self.evict();
            }
            None => {
                self.failed.insert(key);
            }
        }
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

/// Decode `path` and downscale it to a `target`-edge BGRA thumbnail. Blocking;
/// must run on the background executor. Returns `None` for anything it will not
/// or cannot safely render (too large, decompression bomb, decode failure).
fn decode_thumbnail(path: &Path, target: u32) -> Option<Arc<RenderImage>> {
    let (mut file, total) = crate::storage::local().open_read(path).ok()?;
    if total == 0 || total > THUMB_MAX_BYTES {
        return None;
    }

    // Header-only probe first, so a decompression bomb is refused before any
    // pixel buffer is allocated.
    let (w, h) = image::ImageReader::new(BufReader::new(&file))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if u64::from(w) * u64::from(h) > THUMB_MAX_PIXELS {
        return None;
    }

    // Rewind past the probe and decode the full image once.
    file.rewind().ok()?;
    let decoded = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;

    // `thumbnail` is a fast, aspect-preserving downscale bounded by target×target.
    let rgba = decoded.thumbnail(target, target).into_rgba8();
    Some(render_image_from_rgba(rgba))
}
