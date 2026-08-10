//! Markdown preview: a capped head, downgraded to hex when the bytes turn out
//! to be binary.
//!
//! The neutralizer that strips URL sinks out of the source lands in commit 4;
//! this file is currently a straight move of the old loader arm.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::providers::hex::hex_from;
use crate::backend::services::preview::{
    LoadCtx, MARKDOWN_CAP, PreviewKind, PreviewProvider, read, sniff,
};

pub struct Markdown;

impl PreviewProvider for Markdown {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Markdown
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;
        let (bytes, total) = read::read_head(ctx.path, MARKDOWN_CAP)?;
        if sniff::looks_binary(&bytes) {
            return Ok(hex_from(&bytes, total));
        }
        Ok(PreviewPayload::Markdown {
            source: String::from_utf8_lossy(&bytes).as_ref().into(),
            truncated: (bytes.len() as u64) < total,
        })
    }
}
