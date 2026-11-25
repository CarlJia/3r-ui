#!/usr/bin/env bash
# ==============================================================================
# 3R-UI Remote VPS One-Key Deployment Script
#
# Usage:
#   ./scripts/deploy.sh [path/to/deploy.conf]
#
# Default config file path: ./scripts/deploy.conf or ./deploy.conf
# ==============================================================================

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
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

# 1. Locate config file
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

CONF_FILE="${1:-}"

if [[ -z "$CONF_FILE" ]]; then
    if [[ -f "$SCRIPT_DIR/deploy.conf" ]]; then
        CONF_FILE="$SCRIPT_DIR/deploy.conf"
    elif [[ -f "$WORKSPACE_ROOT/deploy.conf" ]]; then
        CONF_FILE="$WORKSPACE_ROOT/deploy.conf"
    elif [[ -f "$SCRIPT_DIR/deploy.conf.example" ]]; then
        warn "deploy.conf not found. Using template: $SCRIPT_DIR/deploy.conf.example"
        warn "Please copy it to deploy.conf and customize: cp $SCRIPT_DIR/deploy.conf.example $SCRIPT_DIR/deploy.conf"
        CONF_FILE="$SCRIPT_DIR/deploy.conf.example"
    else
        error "No configuration file found. Please provide path: ./scripts/deploy.sh <config_file>"
    fi
fi

if [[ ! -f "$CONF_FILE" ]]; then
    error "Configuration file not found: $CONF_FILE"
fi

info "Loading deployment configuration from: $CONF_FILE"
# shellcheck source=/dev/null
source "$CONF_FILE"

# 2. Validate essential config
VPS_HOST="${VPS_HOST:-}"
VPS_PORT="${VPS_PORT:-22}"
VPS_USER="${VPS_USER:-root}"
SSH_KEY="${SSH_KEY:-}"
SSH_EXTRA_ARGS="${SSH_EXTRA_ARGS:--o StrictHostKeyChecking=no -o ConnectTimeout=10}"

if [[ -z "$VPS_HOST" || "$VPS_HOST" == "your-vps-ip" ]]; then
    error "VPS_HOST is not set or still default ('your-vps-ip') in $CONF_FILE. Please set your real VPS IP or domain."
fi

DEPLOY_MODE="${DEPLOY_MODE:-release}"
GITHUB_REPO="${GITHUB_REPO:-CarlJia/3r-ui}"
BIND_ADDR="${BIND_ADDR:-127.0.0.1:2053}"
PUBLIC_HOST="${PUBLIC_HOST:-}"
XRAY_DOWNLOAD_BASE="${XRAY_DOWNLOAD_BASE:-https://api.github.com}"
STATS_INTERVAL_SECS="${STATS_INTERVAL_SECS:-30}"
RUST_LOG="${RUST_LOG:-info,three_r_ui=info}"
AUTO_START_SERVICE="${AUTO_START_SERVICE:-true}"
ENABLE_BBR="${ENABLE_BBR:-true}"
FIREWALL_PORTS="${FIREWALL_PORTS:-}"

# Build SSH / SCP command options
SSH_CMD_OPTS=(-p "$VPS_PORT")
SCP_CMD_OPTS=(-P "$VPS_PORT")

if [[ -n "$SSH_EXTRA_ARGS" ]]; then
    # split extra args
    read -r -a EXTRA_OPTS <<< "$SSH_EXTRA_ARGS"
    SSH_CMD_OPTS+=("${EXTRA_OPTS[@]}")
    SCP_CMD_OPTS+=("${EXTRA_OPTS[@]}")
fi

if [[ -n "$SSH_KEY" ]]; then
    # Expand ~ if present
    SSH_KEY_EXPANDED="${SSH_KEY/#\~/$HOME}"
    if [[ ! -f "$SSH_KEY_EXPANDED" ]]; then
        error "Specified SSH key file does not exist: $SSH_KEY ($SSH_KEY_EXPANDED)"
    fi
    SSH_CMD_OPTS+=(-i "$SSH_KEY_EXPANDED")
    SCP_CMD_OPTS+=(-i "$SSH_KEY_EXPANDED")
fi

run_ssh() {
    ssh "${SSH_CMD_OPTS[@]}" "${VPS_USER}@${VPS_HOST}" "$@"
}

run_scp() {
    scp "${SCP_CMD_OPTS[@]}" "$@"
}

echo -e "${CYAN}========================================================${PLAIN}"
echo -e "${CYAN}             3R-UI Remote VPS Deployer                  ${PLAIN}"
echo -e "${CYAN}========================================================${PLAIN}"
echo " Target VPS:      ${VPS_USER}@${VPS_HOST}:${VPS_PORT}"
echo " Deploy Mode:     ${DEPLOY_MODE}"
echo " Bind Address:    ${BIND_ADDR}"
echo " Public Host:     ${PUBLIC_HOST:-'(Auto/Same as VPS)'}"
echo " Auto-Start:      ${AUTO_START_SERVICE}"
echo -e "${CYAN}--------------------------------------------------------${PLAIN}"

# 3. Test SSH connectivity
info "Testing SSH connection to ${VPS_USER}@${VPS_HOST}:${VPS_PORT}..."
if ! run_ssh "echo 'SSH_CONNECTION_SUCCESS'" >/dev/null 2>&1; then
    error "Failed to connect to ${VPS_USER}@${VPS_HOST}:${VPS_PORT}. Check IP, port, credentials, and firewall."
fi
success "SSH connection established."

# 4. Check remote OS and Architecture
info "Inspecting remote OS and hardware architecture..."
REMOTE_INFO=$(run_ssh '
    ARCH=$(uname -m)
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        OS="$PRETTY_NAME"
        OS_ID="$ID"
    else
        OS="Unknown"
        OS_ID="unknown"
    fi
    echo "$ARCH|$OS_ID|$OS"
')

IFS='|' read -r REMOTE_ARCH REMOTE_OS_ID REMOTE_OS <<< "$REMOTE_INFO"
info "Remote System: $REMOTE_OS (Arch: $REMOTE_ARCH, OS_ID: $REMOTE_OS_ID)"

case "$REMOTE_ARCH" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        RUST_TARGET="x86_64-unknown-linux-musl"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        RUST_TARGET="aarch64-unknown-linux-musl"
        ;;
    *)
        error "Unsupported remote architecture: $REMOTE_ARCH. 3R-UI requires x86_64 or aarch64."
        ;;
esac

# 5. Handle binary preparation based on DEPLOY_MODE
UPLOAD_LOCAL_BIN=""
if [[ "$DEPLOY_MODE" == "build" ]]; then
    info "DEPLOY_MODE=build: Compiling 3r-ui locally for target $RUST_TARGET..."
    if command -v cross >/dev/null 2>&1; then
        cross build --release --target "$RUST_TARGET"
        UPLOAD_LOCAL_BIN="$WORKSPACE_ROOT/target/$RUST_TARGET/release/3r-ui"
    else
        warn "cross not found; attempting cargo build --release..."
        cargo build --release
        UPLOAD_LOCAL_BIN="$WORKSPACE_ROOT/target/release/3r-ui"
    fi
    [[ -f "$UPLOAD_LOCAL_BIN" ]] || error "Local binary not found at $UPLOAD_LOCAL_BIN after build."
elif [[ "$DEPLOY_MODE" == "local" ]]; then
    if [[ -n "${LOCAL_BINARY:-}" && -f "$LOCAL_BINARY" ]]; then
        UPLOAD_LOCAL_BIN="$LOCAL_BINARY"
    elif [[ -f "$WORKSPACE_ROOT/target/release/3r-ui" ]]; then
        UPLOAD_LOCAL_BIN="$WORKSPACE_ROOT/target/release/3r-ui"
    else
        error "LOCAL_BINARY not found. Please compile it first or specify valid path in deploy.conf."
    fi
fi

# 6. Remote system preparation & dependencies
info "Ensuring remote dependencies (curl, tar, ca-certificates, systemd)..."
run_ssh "
    if [ \"\$(id -u)\" -ne 0 ]; then
        SUDO='sudo'
    else
        SUDO=''
    fi
    \$SUDO mkdir -p /etc/3r-ui /var/lib/3r-ui /var/log/3r-ui /usr/local/bin
    \$SUDO chmod 750 /etc/3r-ui /var/lib/3r-ui /var/log/3r-ui

    if command -v apt-get >/dev/null 2>&1; then
        DEBIAN_FRONTEND=noninteractive \$SUDO apt-get update -qq
        DEBIAN_FRONTEND=noninteractive \$SUDO apt-get install -y -qq curl tar ca-certificates systemd >/dev/null
    fi
"

# 7. Upload or download 3R-UI binary
if [[ -n "$UPLOAD_LOCAL_BIN" ]]; then
    info "Uploading local binary ($UPLOAD_LOCAL_BIN) to remote VPS..."
    TMP_REMOTE_BIN="/tmp/3r-ui-upload.$$"
    run_scp "$UPLOAD_LOCAL_BIN" "${VPS_USER}@${VPS_HOST}:${TMP_REMOTE_BIN}"
    run_ssh "
        if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
        \$SUDO mv '${TMP_REMOTE_BIN}' /usr/local/bin/3r-ui
        \$SUDO chmod +x /usr/local/bin/3r-ui
    "
    success "Binary uploaded and installed to /usr/local/bin/3r-ui."
else
    info "DEPLOY_MODE=release: Fetching latest release asset for $TARGET_ARCH on remote VPS..."
    run_ssh "
        if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
        GITHUB_REPO='${GITHUB_REPO}'
        TARGET_ARCH='${TARGET_ARCH}'
        RUST_TARGET='${RUST_TARGET}'

        RELEASE_API=\"https://api.github.com/repos/\${GITHUB_REPO}/releases/latest\"
        DOWNLOAD_URL=\$(curl -fsSL \"\$RELEASE_API\" | grep \"browser_download_url\" | grep \"\$TARGET_ARCH\" | head -n 1 | cut -d '\"' -f 4 || true)

        if [ -z \"\$DOWNLOAD_URL\" ]; then
            DOWNLOAD_URL=\"https://github.com/\${GITHUB_REPO}/releases/latest/download/3r-ui-\${RUST_TARGET}\"
        fi

        echo \"Downloading 3r-ui from \$DOWNLOAD_URL ...\"
        TMP_FILE=\"/tmp/3r-ui-download.\$\$\"
        curl -fsSL -L \"\$DOWNLOAD_URL\" -o \"\$TMP_FILE\"
        \$SUDO mv \"\$TMP_FILE\" /usr/local/bin/3r-ui
        \$SUDO chmod +x /usr/local/bin/3r-ui
    "
    success "Release binary installed to /usr/local/bin/3r-ui."
fi

# 8. Render and push environment configuration
info "Configuring /etc/3r-ui/3r-ui.env on remote VPS..."
EFFECTIVE_PUBLIC_HOST="$PUBLIC_HOST"
if [[ -z "$EFFECTIVE_PUBLIC_HOST" ]]; then
    EFFECTIVE_PUBLIC_HOST="$VPS_HOST"
fi

TMP_ENV_FILE="/tmp/3r-ui-env.$$"
cat <<EOF > "$TMP_ENV_FILE"
# 3R-UI environment configuration generated by deploy.sh
DATA_DIR=/var/lib/3r-ui
LOG_DIR=/var/log/3r-ui
BIND_ADDR=${BIND_ADDR}
PUBLIC_HOST=${EFFECTIVE_PUBLIC_HOST}
XRAY_DOWNLOAD_BASE=${XRAY_DOWNLOAD_BASE}
STATS_INTERVAL_SECS=${STATS_INTERVAL_SECS}
RUST_LOG=${RUST_LOG}
EOF

run_scp "$TMP_ENV_FILE" "${VPS_USER}@${VPS_HOST}:/tmp/3r-ui.env"
rm -f "$TMP_ENV_FILE"

run_ssh "
    if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
    \$SUDO mv /tmp/3r-ui.env /etc/3r-ui/3r-ui.env
    \$SUDO chmod 600 /etc/3r-ui/3r-ui.env
"
success "Configuration synced to /etc/3r-ui/3r-ui.env"

# 9. Sync systemd service file
info "Configuring systemd service..."
TMP_SERVICE_FILE="/tmp/3r-ui-service.$$"
cat <<'EOF' > "$TMP_SERVICE_FILE"
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

run_scp "$TMP_SERVICE_FILE" "${VPS_USER}@${VPS_HOST}:/tmp/3r-ui.service"
rm -f "$TMP_SERVICE_FILE"

run_ssh "
    if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
    \$SUDO mv /tmp/3r-ui.service /etc/systemd/system/3r-ui.service
    \$SUDO chmod 644 /etc/systemd/system/3r-ui.service
    \$SUDO systemctl daemon-reload
    \$SUDO systemctl enable 3r-ui >/dev/null 2>&1 || true
"
success "Systemd service configured and enabled."

# 10. Optional system optimizations: BBR and Firewall
if [[ "$ENABLE_BBR" == "true" ]]; then
    info "Enabling TCP BBR congestion control..."
    run_ssh "
        if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
        if ! sysctl net.ipv4.tcp_congestion_control | grep -q bbr; then
            echo 'net.core.default_qdisc=fq' | \$SUDO tee -a /etc/sysctl.d/99-bbr.conf >/dev/null
            echo 'net.ipv4.tcp_congestion_control=bbr' | \$SUDO tee -a /etc/sysctl.d/99-bbr.conf >/dev/null
            \$SUDO sysctl -p /etc/sysctl.d/99-bbr.conf >/dev/null 2>&1 || true
        fi
    "
fi

if [[ -n "$FIREWALL_PORTS" ]]; then
    info "Configuring firewall for ports: $FIREWALL_PORTS..."
    run_ssh "
        if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
        if command -v ufw >/dev/null 2>&1; then
            for p in ${FIREWALL_PORTS}; do
                \$SUDO ufw allow \"\$p\" >/dev/null 2>&1 || true
            done
        fi
    "
fi

# 11. Start / Restart Service
if [[ "$AUTO_START_SERVICE" == "true" ]]; then
    info "Starting 3R-UI service on remote VPS..."
    run_ssh "
        if [ \"\$(id -u)\" -ne 0 ]; then SUDO='sudo'; else SUDO=''; fi
        \$SUDO systemctl restart 3r-ui
        sleep 2
        \$SUDO systemctl is-active 3r-ui
    "
    success "3R-UI service is active!"

    echo ""
    echo -e "${GREEN}========================================================${PLAIN}"
    echo -e "${GREEN}      3R-UI 远程 VPS 一键部署成功!                     ${PLAIN}"
    echo -e "${GREEN}========================================================${PLAIN}"
    echo " 远程主机:     ${VPS_HOST}"
    echo " 内部监听:     ${BIND_ADDR}"
    echo " 节点公网Host: ${EFFECTIVE_PUBLIC_HOST}"
    echo " 配置文件:     /etc/3r-ui/3r-ui.env"
    echo " 服务管理:     systemctl status 3r-ui"
    echo " 查看日志:     journalctl -u 3r-ui -f"
    echo -e "${GREEN}--------------------------------------------------------${PLAIN}"
    echo " 首次启动管理员密码请通过以下命令在 VPS 查看："
    echo "   ssh -p ${VPS_PORT} ${VPS_USER}@${VPS_HOST} \"journalctl -u 3r-ui -n 50 --no-pager | grep -A 4 'admin password'\""
    echo -e "${GREEN}========================================================${PLAIN}"
else
    success "Deployment files synced. AUTO_START_SERVICE is false; service not started."
fi
