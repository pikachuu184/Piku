#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod core;
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
