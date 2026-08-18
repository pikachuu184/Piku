//! Video preview: container metadata plus a best-effort poster frame.
//!
//! Real metadata for the MP4 family (mp4/mov/m4v) comes from the pure-Rust
//! `mp4` crate — duration, resolution, codecs, frame rate, bitrate — without
//! decoding a frame. Other containers (mkv/webm/avi/…) fall back to the sniffed
//! container name; size and dates already appear in the panel's detail rows.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::{
    LoadCtx, PreviewKind, PreviewProvider, probe, read, sniff,
};

pub struct VideoMeta;

impl PreviewProvider for VideoMeta {
    fn kind(&self) -> PreviewKind {
        PreviewKind::VideoMeta
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;

        let rows = match probe::mp4_metadata(ctx.path) {
            Some(rows) if !rows.is_empty() => rows,
            _ => {
                let (bytes, _total) = read::read_head(ctx.path, 64)?;
                let format = sniff::sniff(&bytes).unwrap_or("Unknown container");
                vec![("Container".into(), format.into())]
            }
        };

        // The poster frame spawns ffmpeg, which is by far the longest thing a
        // preview does. Checking first means arrowing past a folder of videos
        // does not queue one subprocess per file.
        ctx.cancel.check()?;
        let poster = probe::poster_frame(ctx.path, None, ctx.cancel);

        Ok(PreviewPayload::Video { rows, poster })
    }
}
