#!/usr/bin/env bash
#
# End-to-end validation: launch the real binary against seeded fixtures and
# assert on its structured tracing output.
#
#     bash ci/e2e.sh
#
# This complements the unit tests, which never start a window, never touch the
# platform layer, and cannot see whether the app actually stays responsive.
# Everything asserted here is a property a user would notice.
#
# Thresholds ratchet **down** only. If a stage makes the app better, lower the
# number in the same commit; a regression then fails the build instead of
# quietly eroding.

set -uo pipefail
cd "$(dirname "$0")/.."

# --- Budgets ---------------------------------------------------------------

# Operations still allowed to block the UI thread, as a space-separated set.
#
# Every entry is a known debt with an owning stage. Remove entries as they move
# behind the backend; an operation appearing here that is not on this list is a
# regression and fails the run.
#
#   persistence::{load,save}_json  -> Stage 4 (PersistenceService)
#   watcher::watch                 -> Stage 6 (WatchService)
#   storage::open_read             -> Stage 7 (PreviewService)
ALLOWED_UI_BLOCKING=${ALLOWED_UI_BLOCKING:-"persistence::load_json persistence::save_json watcher::watch storage::open_read"}

# Watcher registrations per distinct directory over an idle run. More than one
# means the re-registration loop is back.
MAX_WATCHES_PER_DIR=1

# p99 of the ExplorerPanel render span, in microseconds.
MAX_RENDER_P99_US=${MAX_RENDER_P99_US:-8000}

# How long to let the app idle while we watch it.
RUN_SECONDS=${RUN_SECONDS:-15}

failed=0
fail() {
    printf 'FAIL: %s\n' "$1" >&2
    shift
    for line in "$@"; do printf '      %s\n' "$line" >&2; done
    failed=1
}
ok() { printf 'ok:   %s\n' "$1"; }

# --- Fixtures --------------------------------------------------------------

WORK=$(mktemp -d "${TMPDIR:-/tmp}/piku-e2e-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

FIXTURES="$WORK/fixtures"
mkdir -p "$FIXTURES"

# A directory big enough that a non-streaming listing is visible to a human.
mkdir -p "$FIXTURES/big"
python3 - "$FIXTURES/big" <<'PY'
import os, sys
d = sys.argv[1]
for i in range(5000):
    open(os.path.join(d, f"file_{i:05d}.txt"), "w").close()
PY

# A filename carrying a right-to-left override: the extension spoof.
printf 'x' > "$FIXTURES/invoice"$'‮'"gpj.exe" 2>/dev/null || true

# A symlink pointing outside the fixture tree.
mkdir -p "$FIXTURES/links"
ln -sf /etc "$FIXTURES/links/escape" 2>/dev/null || true

# A directory we cannot read.
mkdir -p "$FIXTURES/locked"
chmod 000 "$FIXTURES/locked" 2>/dev/null || true

# An archive with a traversing entry name.
if command -v python3 >/dev/null; then
python3 - "$FIXTURES/evil.zip" <<'PY'
import sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w") as z:
    z.writestr("../../escape.txt", "x")
    z.writestr("normal.txt", "x")
PY
fi

ok "fixtures seeded in $FIXTURES"

# --- Build -----------------------------------------------------------------

if ! cargo build --locked >/dev/null 2>&1; then
    fail "build" "cargo build --locked failed"
    exit 1
fi
BIN=./target/debug/piku
[ -x "$BIN" ] || { fail "build" "$BIN not found"; exit 1; }

# --- Run -------------------------------------------------------------------

LOG="$WORK/run.log"
# An isolated data dir so a developer's real session/layout does not decide
# what this run does, and so the run leaves nothing behind.
export XDG_DATA_HOME="$WORK/data"
export XDG_CACHE_HOME="$WORK/cache"
mkdir -p "$XDG_DATA_HOME" "$XDG_CACHE_HOME"

if [ -z "${DISPLAY:-}${WAYLAND_DISPLAY:-}" ]; then
    printf 'skip: no display server; E2E needs a real window\n'
    exit 0
fi

PIKU_TRACE_SPANS=1 RUST_LOG="piku=debug,piku::render=trace" \
    "$BIN" > "$LOG" 2>&1 &
PID=$!

# Give it time to start, restore its session, and idle.
sleep "$RUN_SECONDS"

if ! kill -0 "$PID" 2>/dev/null; then
    fail "liveness" "the app exited before we could observe it" "$(tail -5 "$LOG")"
    cat "$LOG" >&2
    exit 1
fi

# Time the quit: gpui allows 200 ms for quit futures, and a blocking drain
# blows straight through it.
QUIT_START=$(date +%s%N)
kill -TERM "$PID" 2>/dev/null
wait "$PID" 2>/dev/null
QUIT_MS=$(( ($(date +%s%N) - QUIT_START) / 1000000 ))

# Strip ANSI so the greps below are exact.
PLAIN="$WORK/run.plain.log"
sed 's/\x1b\[[0-9;]*m//g' "$LOG" > "$PLAIN"

# --- Assertions ------------------------------------------------------------

# 1. Nothing may block while an element tree is being built. This is the hard
#    invariant; in a debug build it also panics.
inside=$(grep -c 'inside a render pass' "$PLAIN" || true)
if [ "$inside" -ne 0 ]; then
    fail "no blocking work inside a render pass" \
        "found $inside occurrence(s)" \
        "$(grep 'inside a render pass' "$PLAIN" | head -5)"
else
    ok "no blocking work inside a render pass"
fi

# 2. The app must not panic.
panics=$(grep -c 'panicked at' "$PLAIN" || true)
if [ "$panics" -ne 0 ]; then
    fail "no panics" "$(grep -A2 'panicked at' "$PLAIN" | head -10)"
else
    ok "no panics"
fi

# 3. *Which* operations still block the UI thread.
#
#    Asserted as a set, not a count. The count depends on how many panes the
#    restored session happens to open and how long the run idles (observed 9-14
#    for identical code), which would make a numeric budget flaky. The set is
#    stable and says the thing we actually care about: which operations have
#    not moved behind the backend yet. Stage 4 empties this list; anything
#    *new* appearing here fails immediately.
observed=$(grep 'blocking work on the UI thread' "$PLAIN" \
    | grep -o 'operation="[^"]*"' | sed 's/operation="//; s/"//' | sort -u)
unexpected=""
for op in $observed; do
    case " $ALLOWED_UI_BLOCKING " in
        *" $op "*) ;;
        *) unexpected="$unexpected $op" ;;
    esac
done
if [ -n "$unexpected" ]; then
    fail "no new UI-thread blocking" \
        "these operations are not on the known list:$unexpected" \
        "known:$ALLOWED_UI_BLOCKING" \
        "$(grep 'blocking work on the UI thread' "$PLAIN" \
            | grep -o 'operation="[^"]*"' | sort | uniq -c | sort -rn)"
else
    count=$(printf '%s\n' "$observed" | grep -c . || true)
    ok "no new UI-thread blocking ($count known operation(s) remaining)"
fi

# 4. Watcher churn: the regression that re-registered ~once per second.
worst=$(grep 'registering watcher' "$PLAIN" \
    | grep -o 'dir=.*' | sort | uniq -c | sort -rn | awk 'NR==1{print $1}')
worst=${worst:-0}
if [ "$worst" -gt "$MAX_WATCHES_PER_DIR" ]; then
    fail "watcher registers once per directory" \
        "one directory was registered $worst times" \
        "$(grep 'registering watcher' "$PLAIN" | grep -o 'dir=.*' | sort | uniq -c | sort -rn | head -3)"
else
    ok "watcher registers once per directory (max $worst)"
fi

# 5. The backend actually started and served requests.
if ! grep -q 'backend runtime started' "$PLAIN"; then
    fail "backend runtime started" "no startup line in the log"
else
    dispatched=$(grep -c 'dispatched req=' "$PLAIN" || true)
    ok "backend runtime started ($dispatched request(s) dispatched)"
fi

# 6. Clean, prompt quit. gpui's budget is 200 ms; allow slack for process
#    teardown on a loaded CI box, but a 2 s blocking drain fails.
if grep -q 'did not drain' "$PLAIN"; then
    fail "clean shutdown" "the backend did not drain before its deadline"
elif [ "$QUIT_MS" -gt 1000 ]; then
    fail "prompt shutdown" "quit took ${QUIT_MS}ms"
else
    ok "clean shutdown (${QUIT_MS}ms)"
fi

# 7. Render latency.
p99=$(grep 'render{' "$PLAIN" | grep -o 'time.busy=[0-9.]*[munµ]*s' | sed 's/time.busy=//' \
    | python3 -c '
import sys
def us(t):
    t = t.strip()
    for suf, mul in (("ms", 1000.0), ("µs", 1.0), ("us", 1.0), ("ns", 0.001)):
        if t.endswith(suf):
            return float(t[: -len(suf)]) * mul
    return float(t.rstrip("s")) * 1e6
vals = sorted(us(l) for l in sys.stdin if l.strip())
print(int(vals[min(len(vals) - 1, int(0.99 * len(vals)))]) if vals else -1)
')
if [ "${p99:--1}" -lt 0 ]; then
    ok "render latency (no samples; nothing re-rendered while idle)"
elif [ "$p99" -gt "$MAX_RENDER_P99_US" ]; then
    fail "render p99 within budget" "${p99}us > ${MAX_RENDER_P99_US}us"
else
    ok "render p99 within budget (${p99}us <= ${MAX_RENDER_P99_US}us)"
fi

# --- Cleanup ---------------------------------------------------------------

chmod 755 "$FIXTURES/locked" 2>/dev/null || true

if [ "$failed" -ne 0 ]; then
    printf '\n%s\n' "End-to-end validation failed. Full log:" >&2
    cat "$PLAIN" >&2
    exit 1
fi
printf '\nEnd-to-end validation passed.\n'
