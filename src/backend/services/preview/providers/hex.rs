//! The universal fallback: a hex + ASCII head with a sniffed signature.
//!
//! Also owns [`hex_from`], which the text providers call when their bytes turn
//! out to be binary — a `.txt` full of NULs is more usefully shown as hex than
//! as mojibake.

use crate::backend::error::PreviewError;
use crate::backend::services::preview::content::{HexRow, PreviewPayload};
use crate::backend::services::preview::{
    HEX_CAP, LoadCtx, PreviewKind, PreviewProvider, read, sniff,
};

pub struct Hex;

impl PreviewProvider for Hex {
    fn kind(&self) -> PreviewKind {
        PreviewKind::Hex
    }

    fn load(&self, ctx: &LoadCtx<'_>) -> Result<PreviewPayload, PreviewError> {
        ctx.cancel.check()?;
        let (bytes, total) = read::read_head(ctx.path, HEX_CAP)?;
        // Extensionless (or mislabeled) files whose magic bytes are a
        // renderable image get upgraded to a real image preview.
        if sniff::sniffed_renderable_image(&bytes) {
            return Ok(PreviewPayload::Image {
                path: ctx.path.to_path_buf(),
                dimensions: None,
            });
        }
        Ok(hex_from(&bytes, total))
    }
}

/// Precompute display rows (offset · 16 hex bytes · ASCII gutter) so the UI
/// thread renders strings only.
pub fn hex_from(bytes: &[u8], total: u64) -> PreviewPayload {
    let rows = bytes
        .chunks(16)
        .take(HEX_CAP / 16)
        .enumerate()
        .map(|(index, chunk)| {
            let mut hex = String::with_capacity(chunk.len() * 3);
            let mut ascii = String::with_capacity(chunk.len());
            for byte in chunk {
                use std::fmt::Write as _;
                let _ = write!(hex, "{byte:02x} ");
                ascii.push(if byte.is_ascii_graphic() || *byte == b' ' {
                    *byte as char
                } else {
                    '·'
                });
            }
            HexRow {
                offset: format!("{:08x}", index * 16).as_str().into(),
                hex: hex.trim_end().into(),
                ascii: ascii.as_str().into(),
            }
        })
        .collect();
    PreviewPayload::Hex {
        rows,
        signature: sniff::sniff(bytes),
        total_size: total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_rows_format() {
        let PreviewPayload::Hex {
            rows,
            signature,
            total_size,
        } = hex_from(b"MZ\x90\x00ABCDEFGHIJKL", 14)
        else {
            unreachable!("hex_from always returns Hex");
        };
        assert_eq!(total_size, 14);
        assert_eq!(signature, Some("Windows executable (PE)"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].offset.as_ref(), "00000000");
        assert!(rows[0].hex.as_ref().starts_with("4d 5a 90 00"));
        assert!(rows[0].ascii.as_ref().starts_with("MZ·"));
    }
}
