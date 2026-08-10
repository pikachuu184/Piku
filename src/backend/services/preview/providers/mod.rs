//! One provider per [`super::PreviewKind`].
//!
//! Each file holds a parser and nothing else. The shared envelope —
//! authorization, the tracing span, the concurrency permit, the cancellation
//! handle — lives in the service that dispatches to them, which is the whole
//! reason these are a trait rather than nine free functions with nine
//! different signatures.
//!
//! Routing stays an exhaustive `match` in [`super::provider_for`]. A registry
//! keyed by kind would return `Option`, and under `#![deny(clippy::panic)]` the
//! miss arm could only degrade silently — turning a missing provider from a
//! compile error into a blank preview.

pub mod archive;
pub mod audio;
pub mod code;
pub mod hex;
pub mod image;
pub mod markdown;
pub mod pdf;
pub mod structured;
pub mod video;
