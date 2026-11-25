#!/usr/bin/env bash
#
# 3R-UI Installer for Debian & Ubuntu (x86_64 / arm64)
#
# Usage:
#   # 1. Install using precompiled binary from latest GitHub release:
#   bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
#
#   # 2. Install using a local uploaded binary or custom path (via argument):
#   bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh) -s /path/to/3r-ui-x86_64-unknown-linux-musl
#
#   # 3. Or via LOCAL_BIN environment variable:
#   LOCAL_BIN=/root/3r-ui-x86_64-unknown-linux-musl bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/install.sh)
#

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
PLAIN='\033[0m'

info() {
    echo -e "${BLUE}[INFO]${PLAIN} $*"
}

success() {
    echo -e "${GREEN}[OK]${PLAIN} $*"
}

warn() {
    echo -e "${YELLOW}[WARN]${PLAIN} $*"
}

error() {
    echo -e "${RED}[ERROR]${PLAIN} $*" >&2
    exit 1
}

# 1. Root check
if [[ $EUID -ne 0 ]]; then
    error "This script must be run as root (or with sudo)."
fi

# 2. OS distribution check
if [[ ! -f /etc/os-release ]]; then
    error "/etc/os-release not found. Unsupported Linux distribution."
fi

. /etc/os-release
OS_ID="${ID:-}"
OS_LIKE="${ID_LIKE:-}"

if [[ "$OS_ID" != "debian" && "$OS_ID" != "ubuntu" && "$OS_LIKE" != *"debian"* && "$OS_LIKE" != *"ubuntu"* ]]; then
    warn "Detected OS is $PRETTY_NAME. This installer is optimized for Debian and Ubuntu."
    read -rp "Continue installation anyway? (y/N): " confirm
    if [[ ! "$confirm" =~ ^[Yy]$ ]]; then
        exit 1
    fi
else
    info "Operating System: $PRETTY_NAME (ID: $OS_ID)"
fi

# 3. Architecture detection
RAW_ARCH="$(uname -m)"
case "$RAW_ARCH" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        RUST_TARGET="x86_64-unknown-linux-musl"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        RUST_TARGET="aarch64-unknown-linux-musl"
        ;;
    *)
        error "Unsupported architecture: $RAW_ARCH. 3R-UI requires 64-bit x86_64 or ARM64 (aarch64)."
        ;;
esac

info "Hardware Architecture: $RAW_ARCH (Mapped target: $TARGET_ARCH / $RUST_TARGET)"

# 4. Prerequisites
info "Checking dependencies (curl, tar, ca-certificates, systemd)..."
if ! command -v systemctl >/dev/null 2>&1; then
    error "systemd is required to run 3R-UI as a service."
fi

MISSING_PKGS=()
for pkg in curl tar ca-certificates; do
    if ! command -v "$pkg" >/dev/null 2>&1; then
        MISSING_PKGS+=("$pkg")
    fi
done

if [[ ${#MISSING_PKGS[@]} -gt 0 ]]; then
    info "Installing missing dependencies: ${MISSING_PKGS[*]}..."
    apt-get update -y
    apt-get install -y "${MISSING_PKGS[@]}"
fi

# 5. Directory layout
INSTALL_DIR="/usr/local/bin"
CONF_DIR="/etc/3r-ui"
DATA_DIR="/var/lib/3r-ui"
LOG_DIR="/var/log/3r-ui"
SERVICE_FILE="/etc/systemd/system/3r-ui.service"
ENV_FILE="$CONF_DIR/3r-ui.env"

info "Setting up directories..."
mkdir -p "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"
chmod 750 "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"

# 6. Binary acquisition
# Accepts argument $1 (or via `bash <(...) -s /path/to/bin`) or environment variable LOCAL_BIN
LOCAL_BIN="${LOCAL_BIN:-${1:-}}"

if [[ -n "$LOCAL_BIN" ]]; then
    if [[ ! -f "$LOCAL_BIN" ]]; then
        error "Specified binary file does not exist: $LOCAL_BIN"
    fi
    info "Using local binary file: $LOCAL_BIN"
    cp -f "$LOCAL_BIN" "$INSTALL_DIR/3r-ui"
elif [[ -f "./target/release/3r-ui" ]]; then
    info "Found workspace release binary at ./target/release/3r-ui"
    cp -f "./target/release/3r-ui" "$INSTALL_DIR/3r-ui"
elif [[ -f "./3r-ui" ]]; then
    info "Found binary in current working directory: ./3r-ui"
    cp -f "./3r-ui" "$INSTALL_DIR/3r-ui"
elif [[ -f "./3r-ui-${RUST_TARGET}" ]]; then
    info "Found matched target binary in current working directory: ./3r-ui-${RUST_TARGET}"
    cp -f "./3r-ui-${RUST_TARGET}" "$INSTALL_DIR/3r-ui"
else
    info "Fetching latest release binary for $TARGET_ARCH from GitHub..."
    GITHUB_REPO="CarlJia/3r-ui"
    RELEASE_API="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
    DOWNLOAD_URL=$(curl -fsSL "$RELEASE_API" | grep "browser_download_url" | grep "$TARGET_ARCH" | head -n 1 | cut -d '"' -f 4 || true)

    if [[ -z "$DOWNLOAD_URL" ]]; then
        # Fallback to direct asset name
        DOWNLOAD_URL="https://github.com/${GITHUB_REPO}/releases/latest/download/3r-ui-${RUST_TARGET}"
    fi

    TMP_BIN="/tmp/3r-ui-download.$$"
    info "Downloading binary from $DOWNLOAD_URL..."
    if ! curl -fsSL -L "$DOWNLOAD_URL" -o "$TMP_BIN"; then
        error "Failed to download binary from $DOWNLOAD_URL. If compiling from source, run: cargo build --release && sudo bash packaging/install.sh"
    fi

    mv "$TMP_BIN" "$INSTALL_DIR/3r-ui"
fi

chmod +x "$INSTALL_DIR/3r-ui"
success "Installed executable to $INSTALL_DIR/3r-ui"

# 7. Environment configuration
if [[ ! -f "$ENV_FILE" ]]; then
    info "Creating default environment file at $ENV_FILE..."
    if [[ -f "./packaging/3r-ui.env" ]]; then
        cp "./packaging/3r-ui.env" "$ENV_FILE"
    else
        cat <<'EOF' > "$ENV_FILE"
# 3R-UI environment configuration
DATA_DIR=/var/lib/3r-ui
LOG_DIR=/var/log/3r-ui
BIND_ADDR=127.0.0.1:2053
PUBLIC_HOST=
XRAY_DOWNLOAD_BASE=https://api.github.com
STATS_INTERVAL_SECS=30
RUST_LOG=info,three_r_ui=info
EOF
    fi
    chmod 600 "$ENV_FILE"
    success "Created configuration file: $ENV_FILE"
else
    info "Existing configuration preserved at $ENV_FILE"
fi

# 8. Systemd service setup
info "Installing systemd service file..."
if [[ -f "./packaging/3r-ui.service" ]]; then
    cp "./packaging/3r-ui.service" "$SERVICE_FILE"
else
    cat <<'EOF' > "$SERVICE_FILE"
[Unit]
Description=3R-UI xray-core management panel
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=root
Group=root
EnvironmentFile=/etc/3r-ui/3r-ui.env
ExecStart=/usr/local/bin/3r-ui
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=read-only
ReadWritePaths=/var/lib/3r-ui /var/log/3r-ui
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
LockPersonality=true
RestrictRealtime=true
RestrictNamespaces=true

StandardOutput=journal
StandardError=journal
SyslogIdentifier=3r-ui

[Install]
WantedBy=multi-user.target
EOF
fi

chmod 644 "$SERVICE_FILE"
systemctl daemon-reload
systemctl enable 3r-ui

# 9. Start service
info "Starting 3R-UI service..."
systemctl restart 3r-ui

sleep 1

if systemctl is-active --quiet 3r-ui; then
    success "3R-UI service is active and running!"
    echo ""
    echo "=========================================================="
    echo " 3R-UI is successfully installed on your system!"
    echo " OS: $PRETTY_NAME ($TARGET_ARCH)"
    echo " Service: systemctl status 3r-ui"
    echo " Logs:    journalctl -u 3r-ui -f"
    echo " Config:  $ENV_FILE"
    echo " Data:    $DATA_DIR"
    echo "=========================================================="
    echo ""
    echo "To view initial administrator credentials, run:"
    echo "  journalctl -u 3r-ui -n 50 --no-pager | grep -A 4 'admin password'"
    echo ""
else
    warn "Service started but status check returned non-active. Check logs:"
    warn "  journalctl -u 3r-ui -n 30 --no-pager"
fi
