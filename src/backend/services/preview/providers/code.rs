//! Source-code and plain-text preview: a capped head, downgraded to hex when
//! the bytes turn out to be binary.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::language::language_for_ext;
use crate::backend::services::preview::providers::hex::hex_from;
use crate::backend::services::preview::{
    CODE_HEAD_CAP, LoadCtx, PreviewKind, PreviewProvider, read, sniff,
};

pub struct Code;

impl PreviewProvider for Code {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Code
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;
        let (bytes, total) = read::read_head(ctx.path, CODE_HEAD_CAP)?;
        if sniff::looks_binary(&bytes) {
            return Ok(hex_from(&bytes, total));
        }
        Ok(PreviewPayload::Code {
            text: String::from_utf8_lossy(&bytes).as_ref().into(),
            language: language_for_ext(ctx.ext),
            truncated: (bytes.len() as u64) < total,
            total_size: total,
        })
    }
}
