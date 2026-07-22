<p align="center">
  <img src="assets/branding/logo.svg" width="140" alt="PIKU logo">
</p>

<p align="center">
  <img src="assets/branding/Screenshot%20(13).png" width="900" alt="PIKU application screenshot">
</p>

<h1 align="center">PIKU</h1>

<p align="center">
  A fast, monochrome, GPU-accelerated desktop file manager built in Rust with GPUI.
</p>

<p align="center">
  <a href="https://github.com/BotCoder254/piku/actions/workflows/ci.yml"><img src="https://github.com/BotCoder254/piku/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <img src="https://img.shields.io/badge/rust-1.85%2B-informational" alt="Rust 1.85+">
  <img src="https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-informational" alt="Platforms">
</p>

---

PIKU is a keyboard-driven file manager with a deliberately monochromatic interface. It pairs a
tabbed, splittable workspace with a built-in preview engine that renders images, PDFs, source code,
structured data, and archives, and plays audio and video without leaving the window. Decoding runs
off the UI thread, every parser treats its input as untrusted, and the whole shell is designed to
stay responsive on multi-gigabyte directories.

## Features

- **Tabbed, splittable workspaces** — independent per-tab sessions (folder, history, sort, zoom,
  filter, search, selection), horizontal and vertical splits, and named workspaces that persist
  across restarts.
- **Preview engine** — images (fit / 1:1 / zoom, transparency checkerboard), PDF pages, syntax-
  highlighted source, JSON/YAML/TOML as a collapsible tree, archive listings, and a hex fallback for
  anything unknown.
- **Media player** — an interactive waveform scrubber, volume and mute, and icon transport controls,
  plus a persistent bottom playback bar and a dockable media panel. Video shows a poster frame and
  opens in your system player.
- **Monochrome by design** — pure luminance-based hierarchy, a single corner-radius token, no
  gradients or accent hues.
- **Hardened I/O** — path sanitization that rejects symlink and junction tricks, size and page caps
  on every decoder, and an append-only audit trail for mutating filesystem operations.
- **Native performance** — GPU-accelerated rendering via GPUI, background decoding, and bounded,
  memory-aware caches.

## Building from source

PIKU builds with a single `cargo build`. The vendored PDF library ships in-tree and the video
thumbnailer binary is fetched automatically at build time (set `PIKU_SKIP_FFMPEG_DOWNLOAD=1` to skip
it for faster development builds).

**Prerequisites**

- [Rust](https://rustup.rs) 1.85 or newer (edition 2024)
- Git

**Windows**

```powershell
rustup default stable-msvc
git clone https://github.com/BotCoder254/piku.git
cd piku
cargo build --release
```

**macOS**

```bash
xcode-select --install   # if the command line tools are not installed
git clone https://github.com/BotCoder254/piku.git
cd piku
cargo build --release
```

**Linux**

Install the GPUI runtime dependencies (package names vary by distribution — the following covers
Debian/Ubuntu), then build:

```bash
sudo apt install build-essential pkg-config libssl-dev libxkbcommon-dev \
  libwayland-dev libxcb1-dev vulkan-tools mesa-vulkan-drivers
git clone https://github.com/BotCoder254/piku.git
cd piku
cargo build --release
```

The compiled binary is written to `target/release/`. Use `cargo run --release` to build and launch
in one step.

## Keyboard shortcuts

| Action | Shortcut |
| --- | --- |
| New tab / duplicate tab | `Ctrl+T` / `Ctrl+Shift+D` |
| Split right / split down | `Ctrl+\` / `Ctrl+Shift+\` |
| Toggle left / right dock | `Ctrl+B` / `Ctrl+Alt+B` |
| Back / forward / up | `Alt+Left` / `Alt+Right` / `Alt+Up` |
| Find in folder | `Ctrl+F` |
| Copy / cut / paste | `Ctrl+C` / `Ctrl+X` / `Ctrl+V` |
| Rename / delete | `F2` / `Delete` |
| New folder / new file | `Ctrl+Shift+N` / `Ctrl+N` |
| Zoom in / out / reset | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md) for the development
workflow, coding conventions, and the checks that run in CI, and use the provided issue and pull
request templates.

## Acknowledgements

Built on [GPUI](https://www.gpui.rs) and the [gpui-component](https://github.com/longbridge/gpui-component)
widget library.
