//! The UI-facing half of the preview engine.
//!
//! The engine itself lives in `crate::backend::services::preview`, where it is
//! covered by the architectural gates and can be cancelled. What remains here
//! is the adapter: the renderable `PreviewContent`, the one conversion into it,
//! and the pixel wrapper that conversion needs.

pub mod content;
pub mod image_util;

pub use crate::backend::services::preview::{PreviewKind, kind_for_path};
