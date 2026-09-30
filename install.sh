#!/usr/bin/env bash
# 3R-UI installer: lays down the binary, data dirs, env file, and a systemd
# unit. Tested on Ubuntu 22.04 / Debian 12. Run from the repo root.
#
#   ./install.sh                 # install from ./target/release/3r-ui
#   ./install.sh --from-release  # download from GitHub releases instead
#   ./install.sh --dry-run       # show what would happen, change nothing
#   ./install.sh --uninstall     # stop the service, remove the unit + files
#
# Required for actual install: root, systemd, internet (for --from-release).

set -euo pipefail

# ---- Paths -----------------------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RELEASE_BIN="${REPO_ROOT}/target/release/3r-ui"

INSTALL_BIN="/usr/local/bin/3r-ui"
DATA_DIR="/var/lib/3r-ui"
LOG_DIR="/var/log/3r-ui"
ETC_DIR="/etc/3r-ui"
UNIT_PATH="/etc/systemd/system/3r-ui.service"
ENV_FILE="${ETC_DIR}/3r-ui.env"
UNIT_TEMPLATE="${REPO_ROOT}/packaging/3r-ui.service"
ENV_TEMPLATE="${REPO_ROOT}/packaging/3r-ui.env"

# ---- Args ------------------------------------------------------------------

DRY_RUN=false
FROM_RELEASE=false
UNINSTALL=false

usage() {
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
    exit "${1:-0}"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) DRY_RUN=true ;;
        --from-release) FROM_RELEASE=true ;;
        --uninstall) UNINSTALL=true ;;
        -h|--help) usage 0 ;;
        *) echo "unknown flag: $1" >&2; usage 1 ;;
    esac
    shift
done

# ---- Logging ---------------------------------------------------------------
#
# `run` prints the command, and executes it unless --dry-run was given. The
# `!` invert lets us keep set -e semantics: a failing real command aborts,
# but a failing echo in dry-run does not.

run() {
    if $DRY_RUN; then
        printf '+ %s\n' "$*"
    else
        "$@"
    fi
}

say() { printf '\033[1;36m[install]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[install]\033[0m %s\n' "$*" >&2; }
die() { printf '\033[1;31m[install]\033[0m %s\n' "$*" >&2; exit 1; }

# ---- Preflight -------------------------------------------------------------

require_root() {
    if [ "$(id -u)" -ne 0 ]; then
        die "install must run as root (try: sudo ./install.sh)"
    fi
}

detect_arch() {
    case "$(uname -m)" in
        x86_64)  echo "x86_64" ;;
        aarch64) echo "arm64"  ;;
        *) die "unsupported architecture: $(uname -m). build from source or open an issue." ;;
    esac
}

# ---- Uninstall -------------------------------------------------------------

do_uninstall() {
    say "stopping service"
    run systemctl stop 3r-ui.service || true
    run systemctl disable 3r-ui.service || true

    say "removing systemd unit"
    run rm -f "${UNIT_PATH}"
    run systemctl daemon-reload

    say "removing installed files"
    run rm -f "${INSTALL_BIN}"
    run rm -f "${ENV_FILE}"

    say "data preserved at ${DATA_DIR} and ${LOG_DIR}; remove manually if intended"
    say "done."
}

# ---- Source binary ---------------------------------------------------------
#
# Local: copy ./target/release/3r-ui.
# Release: download the matching tarball from GitHub. The repo's release
# asset naming convention is `3r-ui-linux-<arch>`; adjust the prefix here if
# the project tags releases differently.

resolve_binary() {
    if $FROM_RELEASE; then
        local arch
        arch="$(detect_arch)"
        local asset="3r-ui-linux-${arch}"
        say "downloading ${asset} from GitHub releases"
        if $DRY_RUN; then
            printf '+ curl -L -o /tmp/%s <release-url>\n' "${asset}"
            return
        fi
        local url
        url="$(curl -fsSL "https://api.github.com/repos/${REPO_OWNER:-3r-ui}/${REPO_NAME:-3r-ui}/releases/latest" \
            | grep -o "\"browser_download_url\": *\"[^\"]*${asset}[^\"]*\"" \
            | head -n1 \
            | cut -d'"' -f4)"
        if [ -z "${url}" ]; then
            die "could not locate release asset for ${asset}"
        fi
        curl -fsSL -o "/tmp/${asset}" "${url}"
        chmod 0755 "/tmp/${asset}"
        RELEASE_BIN="/tmp/${asset}"
    fi

    if [ ! -x "${RELEASE_BIN}" ]; then
        if $DRY_RUN; then
            warn "binary not found at ${RELEASE_BIN} (dry-run continues; build with \`cargo build --release\` before real install)"
            return
        fi
        die "binary not found at ${RELEASE_BIN}; build first (cargo build --release) or pass --from-release"
    fi
}

# ---- Install ---------------------------------------------------------------

do_install() {
    if ! $DRY_RUN; then
        require_root
    fi
    resolve_binary

    say "creating directories"
    run install -d -m 0700 -o root -g root "${DATA_DIR}"
    run install -d -m 0755 -o root -g root "${LOG_DIR}"
    run install -d -m 0755 -o root -g root "${ETC_DIR}"

    say "installing binary to ${INSTALL_BIN}"
    run install -m 0755 -o root -g root "${RELEASE_BIN}" "${INSTALL_BIN}"

    say "writing env file to ${ENV_FILE}"
    if [ -f "${ENV_TEMPLATE}" ]; then
        run install -m 0600 -o root -g root "${ENV_TEMPLATE}" "${ENV_FILE}"
    else
        die "missing env template at ${ENV_TEMPLATE}"
    fi

    say "writing systemd unit to ${UNIT_PATH}"
    if [ -f "${UNIT_TEMPLATE}" ]; then
        run install -m 0644 -o root -g root "${UNIT_TEMPLATE}" "${UNIT_PATH}"
    else
        die "missing unit template at ${UNIT_TEMPLATE}"
    fi

    say "reloading systemd and enabling service"
    run systemctl daemon-reload
    run systemctl enable --now 3r-ui.service

    if $DRY_RUN; then
        say "dry-run complete; no service was actually started."
        say "after a real install the initial admin password is in ${DATA_DIR}/initial_password"
        return
    fi

    # The panel writes its initial admin password on first boot. Wait briefly
    # for the file, then print it (or tell the user to wait).
    say "waiting for ${DATA_DIR}/initial_password (panel needs xray-core to finish its first-start download)"
    local pw=""
    for _ in $(seq 1 60); do
        if [ -s "${DATA_DIR}/initial_password" ]; then
            pw="$(cat "${DATA_DIR}/initial_password")"
            break
        fi
        sleep 2
    done

    echo
    if [ -n "${pw}" ]; then
        echo "======================================================================"
        echo "3R-UI initial admin password: ${pw}"
        echo "URL: http://127.0.0.1:2053/"
        echo "Username: admin"
        echo "Change the password immediately after first login."
        echo "======================================================================"
    else
        warn "initial password not found yet — check 'journalctl -u 3r-ui -f'"
        warn "and '${DATA_DIR}/initial_password' once the panel is up."
    fi
}

# ---- Entry -----------------------------------------------------------------

if $UNINSTALL; then
    do_uninstall
else
    do_install
fi