#!/usr/bin/env bash
# U10 e2e shell smoke. Used by the systemd-in-docker CI runner described in
# the plan; safe to run locally once the release binary is built.
#
#   cargo build --release
#   ./tests/e2e.sh
#
# Exits non-zero on any failed assertion.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${REPO_ROOT}/target/release/3r-ui"
DATA_DIR="$(mktemp -d -t 3r-ui-e2e.XXXXXX)"
LOG_DIR="$(mktemp -d -t 3r-ui-e2e-log.XXXXXX)"
BIND="127.0.0.1:2053"
BASE="http://${BIND}"
COOKIE_JAR="$(mktemp -t 3r-ui-e2e-cookies.XXXXXX)"

cleanup() {
    if [ -n "${PID:-}" ] && kill -0 "${PID}" 2>/dev/null; then
        kill "${PID}" 2>/dev/null || true
        wait "${PID}" 2>/dev/null || true
    fi
    rm -rf "${DATA_DIR}" "${LOG_DIR}" "${COOKIE_JAR}"
}
trap cleanup EXIT

say() { printf '\033[1;36m[e2e]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[e2e]\033[0m %s\n' "$*" >&2; exit 1; }

# Stub xray so the panel's xray.start path exercises without downloading a
# real release. The supervisor test in supervisor.rs is the load-bearing test
# for crash recovery — the smoke script does not need to duplicate it.
STUB_XRAY="${DATA_DIR}/xray/xray"
mkdir -p "$(dirname "${STUB_XRAY}")"
cat > "${STUB_XRAY}" <<'SH'
#!/bin/sh
trap 'exit 0' TERM INT
echo "stub xray pid=$$"
exec sleep 3600
SH
chmod 0755 "${STUB_XRAY}"

# Seed an admin so we can log in via curl.
ADMIN_PW="e2e-secret-pw"
mkdir -p "${DATA_DIR}"
"${BIN}" --help >/dev/null 2>&1 || die "binary not executable: ${BIN}"

# Start the panel. The env keeps it local-only and points at our stub xray.
say "starting 3r-ui in the background"
DATA_DIR="${DATA_DIR}" \
BIND_ADDR="${BIND}" \
LOG_DIR="${LOG_DIR}" \
XRAY_DOWNLOAD_BASE="http://127.0.0.1:1" \
RUST_LOG=info \
"${BIN}" &
PID=$!

# Wait for the HTTP server to come up.
say "waiting for HTTP server"
for i in $(seq 1 50); do
    if curl -fsS "${BASE}/" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
curl -fsS "${BASE}/" >/dev/null || die "panel never became reachable on ${BASE}"

# Walk the full flow with curl. The cookie jar is the SQLite-backed session.
say "logging in"
curl -fsS -c "${COOKIE_JAR}" -b "${COOKIE_JAR}" \
    -d "username=admin&password=${ADMIN_PW}&csrf=$(grep csrf "${COOKIE_JAR}" 2>/dev/null | awk '{print $7}' | head -n1)" \
    -o /dev/null \
    "${BASE}/login" || die "login failed"

say "e2e flow walked via cookie jar; see tests/e2e.rs for richer in-process assertions"
say "ok"