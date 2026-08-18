# Contributing to PIKU

Thank you for your interest in improving PIKU. This document describes how to set up a development
environment, the conventions the codebase follows, and the checks a pull request is expected to pass.

## Development setup

1. Install [Rust](https://rustup.rs) 1.85 or newer (edition 2024) and the platform prerequisites
   listed in the [README](README.md#building-from-source).
2. Fork and clone the repository.
3. Build and run:

   ```bash
   cargo run
   ```

   Set `PIKU_SKIP_FFMPEG_DOWNLOAD=1` to skip fetching the video thumbnailer during development.

## Branching and commits

- Create a topic branch off `main`, named for the change, for example `feat/media-scrubber` or
  `fix/waveform-render`.
- Keep commits focused and write imperative, descriptive commit messages ("Add volume slider", not
  "added stuff").
- Rebase on `main` before opening a pull request so history stays linear.

## Before you open a pull request

Run the same checks CI runs:

```bash
cargo fmt
cargo clippy --all-targets
cargo test
cargo build --release
```

Then open a pull request using the template. Fill in what changed, how it was tested, and which
platforms you verified.

## Coding conventions

- **Match the surrounding code.** Follow the existing module layout, naming, and comment density.
- **Theme.** UI code uses only `cx.theme().*` tokens and the grays defined in
  `src/theme/monochrome.rs`. Corners use `cx.theme().radius`. Do not introduce new colors, hues,
  gradients, or shadows.
- **Untrusted input.** Everything under `src/backend/services/preview/` treats file contents as
  hostile. Providers forbid `unwrap`, `expect`, and `panic`; every one enforces a size or count cap
  and degrades to a typed error or the hex fallback rather than failing. Adding a format means
  adding a `PreviewProvider`, and the table tests in `preview/mod.rs` will require it to refuse a
  directory and to stop when its request is cancelled.
- **Filesystem access.** Read and open files through the sanitizing storage provider
  (`crate::storage::local()`), or, inside the preview engine, through `preview::read` — never with
  raw `std::fs` on user-supplied paths. Mutating operations are recorded in the audit trail.
- **Threading.** Expensive work goes to a backend service and comes back through
  `BackendTask`/`BackendStream`; the UI thread only renders. Do not reach for
  `cx.background_executor().spawn` for new work — it cannot be cancelled, which is the defect
  Stage 7 spent its time removing. If a request can be superseded, hold the `Inflight`.
- **Cancellation.** Any loop that can run longer than a frame checks `cancel.check()?` at its head.
  That includes waiting on a subprocess: `ci/e2e.sh` enforces a prompt shutdown, and an ffmpeg child
  that ignores the shutdown token will either blow the budget or outlive the app.
- **The architectural gates.** `ci/invariants.sh` is mechanical and is meant to stay that way.
  Nothing under `src/backend/services/` may import the renderer crate — *including* its value types,
  which is stricter than it sounds. Payloads cross as plain data (`Arc<str>` for text, raw BGRA for
  pixels) and are converted once, on the UI side, in `src/preview/content.rs`.

## Reporting bugs and requesting features

Use the issue templates. For security-sensitive reports, follow [SECURITY.md](SECURITY.md) instead
of opening a public issue.
