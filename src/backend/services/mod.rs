//! The backend services.
//!
//! **Nothing in this directory may import gpui.** Services take plain data and
//! return plain data (or gpui *value* types like `SharedString`), never `App`,
//! `Context`, `Entity`, `Window`, or `BackgroundExecutor`. That is what lets
//! them be unit-tested without a window and reused on any platform;
//! `ci/invariants.sh` fails the build if the rule is broken.

pub mod drive;
