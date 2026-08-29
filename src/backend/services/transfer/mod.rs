//! The transfer engine: copy, move, and permanent delete.
//!
//! # Why this is not "a progress bar with a thread behind it"
//!
//! A file manager's transfer subsystem has to answer four questions the naive
//! shape cannot:
//!
//! * *How fast is it going, and how long is left?* Needs sampling over time,
//!   not a byte counter.
//! * *Can I stop it without losing what it already did?* Needs a suspension
//!   point the worker actually reaches, not a killed thread.
//! * *What happens when a name is taken?* Needs one decision surface, resolved
//!   before bytes move, not a dialog per collision.
//! * *What happens when I start three at once?* Needs a bounded scheduler; more
//!   workers on one spindle are slower, not faster.
//!
//! # Shape
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`job`] | `JobId`, `TransferKind`, `Priority`, `TransferState`, the request and the summary |
//! | [`conflict`] | `Conflict`, `ConflictPolicy`, `Decision`, `ConflictResolver` |
//! | [`control`] | `TransferControl` — the shared cell the UI writes and the worker reads |
//! | [`rate`] | the throughput ring buffer, and the median ETA |
//! | [`enumerate`] | the scan pass: totals and collisions, before anything moves |
//! | [`precheck`] | free space, identity, self-descendant, writability |
//! | [`copy`] | same-volume rename fast path, otherwise a gated chunked copy |
//! | [`scheduler`] | priority queues, the permit pool, and the 200 ms sample tick |
//! | [`pipeline`] | the `TransferCompleted` publish seam |
//! | [`service`] | the handle the UI holds: `submit`, `pause`, `resume`, `cancel`, `resolve`, `prioritize` |
//!
//! # The two ways a transfer stops
//!
//! These are never conflated, and the difference is behavioral rather than
//! cosmetic:
//!
//! * [`TransferState::Paused`](job::TransferState::Paused) — the worker reached
//!   a safe suspension point and stopped reading and writing. Resume is
//!   automatic and needs no answer from anyone.
//! * [`TransferState::WaitingForInput`](job::TransferState::WaitingForInput) —
//!   the transfer deliberately stopped for a decision. It cannot leave that
//!   state until a [`Decision`](conflict::Decision) arrives.
//!
//! `job::tests::the_two_stopped_states_are_not_interchangeable` pins it.

// The engine is built bottom-up: the scan, the prechecks, and the byte-moving
// pass are here and unit-tested, but nothing calls them until `service.rs` and
// `scheduler.rs` land and `Backend::new` hands out a `TransferService`. Until
// then most of the API surface is reachable only from tests, which is what
// dead-code analysis reports.
//
// `expect` rather than `allow`, and on the parent rather than each module: once
// the UI is wired the attribute itself starts erroring, which is the reminder to
// delete it.
#![expect(dead_code)]

pub mod conflict;
pub mod control;
pub mod copy;
pub mod enumerate;
pub mod job;
pub mod pipeline;
pub mod precheck;
pub mod rate;
pub mod scheduler;
pub mod service;

pub use service::TransferService;
