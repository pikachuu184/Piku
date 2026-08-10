//! The backend services.
//!
//! **Nothing in this directory may import gpui — including its value types.**
//! An earlier version of this note said `SharedString` and friends were fine.
//! They are not: gate 1 in `ci/invariants.sh` greps for an import of the
//! renderer crate by name, and importing `SharedString` from it matches. The
//! gate is right and the note was wrong, so the note changed.
//!
//! (This paragraph cannot spell the pattern out, because writing it would trip
//! the gate — which is itself the demonstration that the gate is not vacuous.)
//!
//! Services take plain data and return plain data. Where a payload has to
//! become a gpui type, it crosses as something cheap to convert and the
//! conversion happens once, on the UI side:
//!
//! * text as `Arc<str>` — `SharedString` is a `SmolStr`, and building one from
//!   an `Arc<str>` bumps a refcount rather than copying;
//! * pixels as a raw BGRA buffer — wrapping one as a `RenderImage` is a move.
//!
//! That is what lets services be unit-tested without a window and reused on any
//! platform.

pub mod drive;
pub mod preview;
