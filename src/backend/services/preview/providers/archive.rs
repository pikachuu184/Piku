//! ZIP preview: the central directory is read, entry data never is.
//!
//! Listing an archive is not extracting one. Nothing here decompresses a byte,
//! so the decompression-ratio and Zip Slip classes of attack are out of scope
//! by construction rather than by a check that could be forgotten. What *is* in
//! scope is that entry names are attacker-controlled text which lands directly
//! in the UI — see the sanitization below.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::{ArchiveItem, PreviewPayload};
use crate::backend::services::preview::{
    ARCHIVE_ENTRY_CAP, LoadCtx, PreviewKind, PreviewProvider, read,
};

/// Cancellation is checked every this many entries. Names are cheap to read,
/// so a per-entry check would cost more than it saves.
const CANCEL_EVERY: usize = 64;

pub struct Archive;

impl PreviewProvider for Archive {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Archive
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;
        let (file, _total) = read::open_read(ctx.path)?;

        let mut archive = zip::ZipArchive::new(file).map_err(|error| {
            PreviewError::Undecodable(format!("not a readable zip: {error}").as_str().into())
        })?;

        let total_count = archive.len();
        let listed = total_count.min(ARCHIVE_ENTRY_CAP);
        let mut entries = Vec::with_capacity(listed);
        for index in 0..listed {
            if index.is_multiple_of(CANCEL_EVERY) {
                ctx.cancel.check()?;
            }
            let Ok(entry) = archive.by_index_raw(index) else {
                continue;
            };
            // `enclosed_name()` is the zip crate's own path-safety accessor: it
            // returns `None` for names that would escape the extraction root
            // (`../`, absolute, drive prefixes). We only *list* archives today,
            // so this is a display concern rather than a write one — but
            // showing the safe form means the listing cannot claim a path the
            // extractor would refuse, and falling back to the raw name keeps
            // hostile entries visible rather than silently hidden.
            //
            // Either way the text is sanitized: a stored name can carry ANSI
            // escapes, newlines, or a bidi override, and it is rendered
            // directly.
            let raw = entry
                .enclosed_name()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.name().to_string());
            entries.push(ArchiveItem {
                name: crate::security::text::sanitize_label(&raw).as_str().into(),
                size: entry.size(),
                is_dir: entry.is_dir(),
            });
        }
        Ok(PreviewPayload::Archive {
            entries,
            total_count,
            truncated: total_count > listed,
        })
    }
}
