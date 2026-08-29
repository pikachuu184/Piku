#!/usr/bin/env bash
#
# Architectural invariants that the compiler cannot enforce inside a single
# crate. Run by CI, and runnable by hand:
#
#     bash ci/invariants.sh
#
# Each check prints one line and, on violation, the offending matches. Exits
# non-zero if any check failed (all checks always run, so one commit surfaces
# every violation rather than one per push).

set -uo pipefail
cd "$(dirname "$0")/.."

failed=0

# Report a violation: name, explanation, and the matches themselves.
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    printf '      %s\n' "$2" >&2
    printf '%s\n' "$3" | sed 's/^/      /' >&2
    failed=1
}

ok() { printf 'ok:   %s\n' "$1"; }

# --- 1. Backend services stay renderer-agnostic ---------------------------
# The services must be portable across Windows/Linux/macOS and reusable
# without a UI. Only `backend/dispatch.rs` is allowed to know gpui exists.
# (Until the tree exists this is vacuously true, which is fine — it starts
# guarding the moment Stage 1 lands.)
check_no_gpui_in_services() {
    local name="backend services do not import gpui"
    if [ ! -d src/backend/services ]; then
        ok "$name (no src/backend/services yet)"
        return
    fi
    local hits
    hits=$(grep -rn '^use gpui\|^use gpui_component\|gpui::' src/backend/services/ || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Services must not depend on the renderer. Move the gpui-facing part to src/backend/dispatch.rs or the ui/ layer." \
            "$hits"
    else
        ok "$name"
    fi
}

# --- 2. Blocking work propagates its tracing span -------------------------
# `tokio::task::spawn_blocking` and `rayon::spawn` do NOT carry the current
# span into the worker. Everything must go through the helper in runtime.rs
# that captures `Span::current()` and re-enters it, or operations silently
# lose their parent span and request id.
check_spawn_blocking_confined() {
    local name="spawn_blocking is confined to backend/runtime.rs"
    local hits
    # Match the call form only. A bare word search also hits doc comments that
    # legitimately *explain* the rule, which would make the gate unfixable.
    hits=$(grep -rn '\.spawn_blocking(' src/ --include='*.rs' \
        | grep -v '^src/backend/runtime.rs:' || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Use backend::runtime::blocking(), which re-enters the current tracing span inside the worker." \
            "$hits"
    else
        ok "$name"
    fi
}

# --- 3. Persisted state holds no credentials ------------------------------
# PIKU handles no secrets today, and src/state/ is documented as storing
# "paths and layout only — never credentials". That is why there is no
# zeroize dependency. This fires the day that stops being true — which is
# the day secret handling actually needs a decision.
check_no_credentials_in_state() {
    local name="persisted state holds no credentials"
    local hits
    hits=$(grep -rniE '\b(password|passphrase|secret|api_key|access_token|refresh_token)\b' \
        src/state/ --include='*.rs' || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Persisted state must not carry secrets. If this is intentional, add zeroize and revisit the threat model before removing this check." \
            "$hits"
    else
        ok "$name"
    fi
}

# --- 4. No string-interpolated SQL ----------------------------------------
# There is no database today. If one ever lands (a search index is the
# likely reason), every query must use bound parameters. This catches the
# concatenation shape on the very first commit that introduces it.
check_no_interpolated_sql() {
    local name="no string-interpolated SQL"
    local hits
    hits=$(grep -rnE '(execute|query|query_row|query_map|prepare)[[:space:]]*\([[:space:]]*&?format!' \
        src/ --include='*.rs' || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Use bound parameters, never format!/concatenation, to build SQL." \
            "$hits"
    else
        ok "$name"
    fi
}

# --- 5. No raw filesystem paths reach the screen --------------------------
# A path carries every ancestor directory's name, all of it filesystem-
# supplied, so one `U+202E` upstream reorders the rendered row for everything
# beneath it (see src/security/text.rs). `FsEntry::name` is cleaned at
# construction but the path deliberately is not — it keeps the real bytes,
# because it is what gets opened. So the ui/ layer has to clean it at the
# point of display, via sanitize_path/sanitize_label.
#
# Element keys and cache keys are a legitimate raw use: nothing draws them.
# Those lines carry an explicit `raw-path-ok:` marker with the reason, the
# same escape shape gate 2's comment argues for.
check_no_raw_paths_in_ui() {
    local name="ui does not render raw filesystem paths"
    local hits
    hits=$(grep -rn '\.display()\|to_string_lossy()' src/ui/ --include='*.rs' \
        | grep -v 'raw-path-ok:' \
        | grep -v 'sanitize_path\|sanitize_label\|sanitize_display' || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Render paths through security::text::sanitize_path (or sanitize_label for one component). If the value is never drawn, append a 'raw-path-ok: <reason>' comment on that line." \
            "$hits"
    else
        ok "$name"
    fi
}

# --- 6. External programs are never handed a shell or an interpolated arg --
# A filename is attacker-controlled input: it can contain spaces, quotes,
# semicolons, backticks, newlines, and `$(...)`. Two rules keep it from
# becoming a command.
#
#   a) Never spawn a shell. `Command::new("sh").arg("-c")` re-parses whatever
#      it is given, so a filename becomes syntax. Spawn the target program
#      directly and pass each argument as its own `.arg()` — Rust's process
#      API hands them to `execvp` as a vector, with no parsing step.
#   b) Never build one argument with `format!`. Even without a shell, a glued
#      `--input=<path>` string can start with `-` and be read as another
#      option. Structured arguments keep the boundary explicit.
#
# The gate is not vacuous: every `Command::new` in the tree today passes
# `ffmpeg_sidecar::paths::ffmpeg_path()` as the executable and its paths as
# separate `.arg()` values. This is what keeps that true when an archive or
# OCR helper arrives.
#
# `format!` inside `.arg(` is occasionally right — a numeric flag value like
# `.arg(format!("{fps}"))` has no path in it. Those lines carry an explicit
# `arg-format-ok:` marker with the reason, the same escape shape gate 5 uses.
check_no_shell_interpolation() {
    local name="external programs get argument arrays, never a shell"
    local shells args hits
    shells=$(grep -rnE 'Command::new\([[:space:]]*"(sh|bash|zsh|dash|cmd|cmd\.exe|powershell|powershell\.exe|pwsh)"' \
        src/ --include='*.rs' || true)
    args=$(grep -rnE '\.arg\([[:space:]]*&?format!' src/ --include='*.rs' \
        | grep -v 'arg-format-ok:' || true)
    # `sort -u`: a single bad line usually trips both sub-checks at once, and
    # reporting it twice reads as two problems.
    hits=$(printf '%s\n%s' "$shells" "$args" | grep -v '^$' | sort -u || true)
    if [ -n "$hits" ]; then
        fail "$name" \
            "Spawn the program directly and pass each argument as its own .arg(), so a filename cannot become syntax. If an interpolated argument provably contains no path, append an 'arg-format-ok: <reason>' comment on that line." \
            "$hits"
    else
        ok "$name"
    fi
}

check_no_gpui_in_services
check_spawn_blocking_confined
check_no_credentials_in_state
check_no_interpolated_sql
check_no_raw_paths_in_ui
check_no_shell_interpolation

if [ "$failed" -ne 0 ]; then
    printf '\n%s\n' "One or more architectural invariants were violated." >&2
    exit 1
fi
printf '\nAll architectural invariants hold.\n'
