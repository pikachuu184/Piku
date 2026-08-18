//! JSON / YAML / TOML / XML preview.
//!
//! The pretty-print happens here, on a worker, precisely so the UI thread never
//! parses a document it is about to render.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::language::language_for_ext;
use crate::backend::services::preview::providers::hex::hex_from;
use crate::backend::services::preview::{
    LoadCtx, PreviewKind, PreviewProvider, STRUCTURED_CAP, read, sniff,
};

pub struct Structured;

impl PreviewProvider for Structured {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Structured
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;
        let (bytes, total) = read::read_head(ctx.path, STRUCTURED_CAP)?;
        if sniff::looks_binary(&bytes) {
            return Ok(hex_from(&bytes, total));
        }
        let truncated = (bytes.len() as u64) < total;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Only for complete documents — a truncated head is not valid JSON.
        let pretty = (ctx.ext == "json" && !truncated)
            .then(|| {
                serde_json::from_str::<serde_json::Value>(&text)
                    .ok()
                    .and_then(|value| serde_json::to_string_pretty(&value).ok())
            })
            .flatten()
            .map(|p| p.as_str().into());
        Ok(PreviewPayload::Structured {
            text: text.as_str().into(),
            language: language_for_ext(ctx.ext),
            pretty,
            truncated,
        })
    }
}
