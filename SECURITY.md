# Security policy

## Reporting a vulnerability

Please do not open a public issue for security-sensitive reports. Instead, use GitHub's private
[security advisory](https://github.com/BotCoder254/piku/security/advisories/new) form to disclose the
issue privately.

Include the affected version or commit, a description of the vulnerability, and, where possible, a
minimal reproduction. You can expect an acknowledgement and an initial assessment, and coordinated
disclosure once a fix is available.

## Scope

PIKU parses untrusted files (images, PDFs, archives, audio, and video). Reports about parser
crashes, resource exhaustion, path traversal, or any way to escape the sanitized storage boundary
are especially valuable.

## Properties we intend to hold

These are the claims worth trying to break. Each is enforced in code and pinned by a test, so a
counterexample is a bug rather than a design discussion.

**PIKU makes no network requests.** The application installs a deny-all HTTP client at startup
(`src/app/http.rs`), so any outbound request is refused and logged. This matters because the
renderer's image element fetches remote URLs, and a Markdown document's images become such URLs
unconditionally — so without this, previewing a file could reach an attacker's server. Git is the
one component that talks to the network, over its own transport, and only when you ask it to.

**Previewing a file is not opening it.** Preview providers read a bounded head, never execute,
never follow a symlink, and never extract an archive. Markdown documents are rewritten before
rendering to remove image and raw-HTML nodes and to defang links whose scheme is not
`http`/`https`/`mailto` — a link click reaches the system opener, so a `file://` or app-scheme URL
in a document you merely looked at is in scope.

**A hostile file cannot exhaust memory or abort the process.** Image decoding is bounded by a
strict header probe, strict per-axis dimension limits, and an allocation budget, all applied before
any pixel buffer exists. Documents nested deeply enough to overflow the parser's stack are refused
before parsing rather than caught afterwards, because a stack overflow aborts and cannot be caught.
Decoded previews are held in a cache bounded by a byte budget as well as an entry count, and an
image is billed the real size of its pixel buffer — an estimate that under-counts is the same bug
as no budget at all.

### A known boundary: SVG

SVG is the one preview whose bytes PIKU does not parse. The `image` crate cannot rasterize it, so
the renderer does, which means neither the decode budget nor the per-axis dimension limits apply to
it — those bound *our* decoder, and our decoder never runs. What PIKU does enforce is the same
refusals every other provider gets — a symlink is never followed, a fifo or device node is never
opened — plus a file-size gate, since size is the only bound left on work we do not perform. The
rasterizer's own resource use is not something this codebase limits. Reports of an SVG that hangs
or exhausts the renderer are in scope and interesting; they are just not a check we can currently
place in front of it.

**Superseded work stops.** Selecting a different file cancels the previous preview, including any
subprocess it started. A report that some operation keeps running, or keeps a subprocess alive after
the app quits, is in scope.
