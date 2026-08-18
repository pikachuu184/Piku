//! Markdown preview: a capped head, downgraded to hex when the bytes turn out
//! to be binary, and neutralized before it ever reaches the renderer.
//!
//! The neutralization is not cosmetic. gpui-component turns every markdown
//! image into a URL fetch through gpui's HTTP client, so a previewed `.md`
//! could reach out to an attacker's server, and a link click is handed to
//! `xdg-open`/`ShellExecute`. See [`markdown_safe`] for the full chain.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::PreviewPayload;
use crate::backend::services::preview::providers::hex::hex_from;
use crate::backend::services::preview::{
    LoadCtx, MARKDOWN_CAP, PreviewKind, PreviewProvider, markdown_safe, read, sniff,
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
        let raw = String::from_utf8_lossy(&bytes);
        let truncated = (bytes.len() as u64) < total;

        match markdown_safe::neutralize(&raw) {
            markdown_safe::Neutralized::Safe(source, report) => {
                if !report.is_empty() {
                    tracing::debug!(
                        target: "piku::preview",
                        images = report.images_removed,
                        html = report.html_removed,
                        links = report.links_defanged,
                        "neutralized markdown before rendering"
                    );
                }
                Ok(PreviewPayload::Markdown {
                    source: source.as_str().into(),
                    truncated,
                })
            }
            // Refused. Fall back to the syntax-highlighted source, which is a
            // view the Markdown preview already offers via its Raw toggle — so
            // this degrades to something the user recognises rather than to an
            // error. Critically it is *not* handed to the Markdown renderer,
            // which parses with the same parser and would hit the same wall.
            markdown_safe::Neutralized::RenderAsPlainText(reason) => {
                tracing::info!(
                    target: "piku::preview",
                    reason,
                    "refusing to render markdown; showing source"
                );
                Ok(PreviewPayload::Code {
                    text: raw.as_ref().into(),
                    language: Some("markdown"),
                    truncated,
                    total_size: total,
                })
            }
        }
    }
}
