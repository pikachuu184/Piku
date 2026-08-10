//! The UI-facing half of the preview engine.
//!
//! The engine itself lives in `crate::backend::services::preview`, where it is
//! covered by the architectural gates and can be cancelled. This module is the
//! adapter: it re-exports the vocabulary the UI needs and owns the one
//! conversion from plain-data payloads into renderable gpui values.

pub mod content;
pub mod image_util;

use std::path::Path;

use crate::backend::protocol::Cancel;
use crate::backend::services::preview as engine;
use content::PreviewContent;

pub use engine::{PreviewKind, decide_kind, kind_for_path};

/// Load a preview synchronously, on whatever thread the caller is on.
///
/// **Migration shim.** The engine has moved but the call sites have not yet;
/// the next commit replaces this with `PreviewService::preview`, at which point
/// the work runs on the backend runtime and can actually be cancelled. Until
/// then this preserves today's behaviour exactly, including the fact that a
/// superseded selection keeps decoding — which is the defect being fixed, not
/// one being introduced.
pub fn load_preview(kind: PreviewKind, path: &Path, ext: &str) -> PreviewContent {
    match engine::load(kind, path, ext, &Cancel::never()) {
        Ok(payload) => PreviewContent::from(payload),
        // Matches the old loader's behaviour: every failure was already an
        // in-band `PreviewContent::Error` rendered as a message box.
        Err(error) => PreviewContent::Error(error.to_string().into()),
    }
}
