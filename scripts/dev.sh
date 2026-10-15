#!/usr/bin/env bash
# 3r-ui — local dev launcher (macOS-friendly).
#
# What it does:
#   1. Builds the release binary if missing (or `--rebuild`).
#   2. Picks a free TCP port (or honours --port).
#   3. Creates a fresh DATA_DIR under .run/ (or honours --data-dir).
#   4. Spawns the binary in the background and tails the initial password banner.
#   5. Prints the URL + admin password + log path. Ctrl-C tears the child down.
#
# Usage:
#   scripts/dev.sh                      # build if needed, fresh data dir, foreground
#   scripts/dev.sh --rebuild           # force rebuild
#   scripts/dev.sh --port 8080        # explicit port
#   scripts/dev.sh --data-dir ./run   # keep state across runs
#   scripts/dev.sh --open              # also open the browser on macOS

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

REBUILD=false
EXPLICIT_PORT=""
EXPLICIT_DATA=""
OPEN_BROWSER=false
PUBLIC_HOST=""

while [ $# -gt 0 ]; do
    case "$1" in
        --rebuild)   REBUILD=true ;;
        --port)      EXPLICIT_PORT="$2"; shift ;;
        --data-dir)  EXPLICIT_DATA="$2"; shift ;;
        --open)      OPEN_BROWSER=true ;;
        --public-host) PUBLIC_HOST="$2"; shift ;;
        -h|--help)
            sed -n '2,16p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
    shift
done

# ---- 1. Build ----------------------------------------------------------

BIN="$REPO_ROOT/target/release/3r-ui"
if [ "$REBUILD" = true ] || [ ! -x "$BIN" ]; then
    echo "[dev] building release binary..."
    cargo build --release
fi
test -x "$BIN" || { echo "[dev] binary missing at $BIN" >&2; exit 1; }

# ---- 2. Pick a free port -----------------------------------------------

free_port() {
    python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

if [ -n "$EXPLICIT_PORT" ]; then
    PORT="$EXPLICIT_PORT"
else
    PORT="$(free_port)"
fi

# ---- 3. Data dir -------------------------------------------------------

if [ -n "$EXPLICIT_DATA" ]; then
    DATA_DIR="$EXPLICIT_DATA"
    mkdir -p "$DATA_DIR"
    KEEP=true
else
    DATA_DIR="$REPO_ROOT/.run/3r-ui-$(date +%Y%m%d-%H%M%S)-$$"
    KEEP=false
fi

LOG="$DATA_DIR/panel.log"
mkdir -p "$DATA_DIR"
echo "[dev] DATA_DIR=$DATA_DIR"

# Tear down any prior instance that owns this data dir. The lock file is
# released by the previous process; the lock survives via fs2 because the
# previous process held it on the OS file descriptor.
if [ -f "$DATA_DIR/3r-ui.lock" ]; then
    echo "[dev] WARNING: $DATA_DIR/3r-ui.lock already exists — reusing a directory from a prior run?"
fi

# ---- 4. Spawn ----------------------------------------------------------

echo "[dev] starting 3r-ui on 127.0.0.1:$PORT"
# Build env explicitly so an unset PUBLIC_HOST does not leak into the
# binary's environment as an empty var (and so a missing conditional does
# not become a stray command argument).
#
# XRAY_DOWNLOAD_BASE defaults to an unreachable address so a local run never
# pulls a multi-MB xray-core release by surprise (startup continues without a
# binary — see `ensure_xray_binary` in src/lib.rs). Export your own value to
# exercise the real update path from /admin/settings, e.g.:
#   XRAY_DOWNLOAD_BASE=https://github.com make dev
env_args=(
    "DATA_DIR=$DATA_DIR"
    "BIND_ADDR=127.0.0.1:$PORT"
    "STATS_INTERVAL_SECS=10"
    "XRAY_DOWNLOAD_BASE=${XRAY_DOWNLOAD_BASE:-http://127.0.0.1:1/dev-invalid}"
    "RUST_LOG=info"
)
if [ -n "$PUBLIC_HOST" ]; then
    env_args+=("PUBLIC_HOST=$PUBLIC_HOST")
fi
env "${env_args[@]}" "$BIN" > "$LOG" 2>&1 &
PID=$!

cleanup() {
    if [ -n "${PID:-}" ] && kill -0 "$PID" 2>/dev/null; then
        # Try TERM first; the binary exits cleanly on Ctrl-C (SIGINT)
        # but tokio does not register a SIGTERM handler by default, so we
        # follow up with INT to mirror what a real Ctrl-C sends.
        kill -TERM "$PID" 2>/dev/null || true
        for _ in $(seq 1 25); do
            kill -0 "$PID" 2>/dev/null || break
            sleep 0.2
        done
        if kill -0 "$PID" 2>/dev/null; then
            kill -INT "$PID" 2>/dev/null || true
            sleep 0.2
        fi
        if kill -0 "$PID" 2>/dev/null; then
            kill -KILL "$PID" 2>/dev/null || true
        fi
        wait "$PID" 2>/dev/null || true
    fi
    if [ "${KEEP:-false}" = false ] && [ -n "${DATA_DIR:-}" ] && [ -d "$DATA_DIR" ]; then
        rm -rf "$DATA_DIR"
    fi
}
trap cleanup EXIT INT TERM HUP

# Wait for the binary to bind (or fail to bind).
for i in $(seq 1 50); do
    if grep -q "3r-ui ready" "$LOG" 2>/dev/null; then break; fi
    if ! kill -0 "$PID" 2>/dev/null; then
        echo "[dev] panel exited before becoming ready; log tail:" >&2
        tail -n 40 "$LOG" >&2
        exit 1
    fi
    sleep 0.2
done
grep -q "3r-ui ready" "$LOG" || { echo "[dev] panel never reported ready" >&2; tail -n 40 "$LOG" >&2; exit 1; }

# ---- 5. Report ---------------------------------------------------------

URL="http://127.0.0.1:$PORT/"
PASSWORD="$(awk '/^Password: / { print $2; exit }' "$LOG")"

cat <<EOF

======================================================================
  3r-ui is running
  URL:    $URL
  User:   admin
  Pass:   $PASSWORD
  Data:   $DATA_DIR
  Log:    $LOG
  PID:    $PID
======================================================================
  Ctrl-C stops the server and (by default) removes the data dir.
  Pass --data-dir <path> to keep state across runs.
======================================================================

EOF

if [ "$OPEN_BROWSER" = true ] && command -v open >/dev/null 2>&1; then
    open "$URL"
fi

# Tail the log so Ctrl-C cleanly tears it down. `tail -f` would block the
# trap on signal; `-F` is robust to log truncation.
tail -n +1 -F "$LOG" &
TAIL_PID=$!
wait "$PID" || true
kill "$TAIL_PID" 2>/dev/null || true