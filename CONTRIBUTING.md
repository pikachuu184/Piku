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
- **Untrusted input.** Everything under `src/preview/` treats file contents as hostile. The loader
  (`src/preview/loader.rs`) forbids `unwrap`, `expect`, and `panic`; every decoder enforces a size
  or count cap and degrades gracefully rather than failing.
- **Filesystem access.** Read and open files through the sanitizing storage provider
  (`crate::storage::local()`), never with raw `std::fs` on user-supplied paths. Mutating operations
  are recorded in the audit trail.
- **Threading.** Decoding and other expensive work runs on the background executor; the UI thread
  only renders.

## Reporting bugs and requesting features

Use the issue templates. For security-sensitive reports, follow [SECURITY.md](SECURITY.md) instead
of opening a public issue.
