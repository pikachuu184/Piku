#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
// PIKU parses untrusted files. There is no `unsafe` anywhere under `src/`, and
// this makes that a property the compiler enforces rather than a convention.
// (`build.rs` is a separate crate and is not covered.)
#![forbid(unsafe_code)]
// Promoted from the file-scoped set that used to live only in
// `preview/loader.rs`: nothing in the shipping binary may panic its way out of
// a bad file, a missing device, or a race. `await_holding_*` are the ones that
// start mattering now that there is an async backend with shared caches.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::todo,
    clippy::unimplemented,
    clippy::dbg_macro,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::mem_forget,
    clippy::await_holding_lock,
    clippy::await_holding_refcell_ref
)]
// Tests may panic freely — that is how they report failure — and they print to
// stderr to explain why a hardware/tool-dependent case self-skipped.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stdout,
        clippy::print_stderr
    )
)]

mod app;
mod backend;
mod core;
mod preview;
mod security;
mod services;
mod state;
mod storage;
mod theme;
mod ui;

fn main() {
    app::init_tracing();
    app::run();
}
