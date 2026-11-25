#!/usr/bin/env bash
# ==============================================================================
# 3R-UI One-Key Uninstaller for Debian & Ubuntu
#
# Usage:
#   bash <(curl -fsSL https://raw.githubusercontent.com/CarlJia/3r-ui/main/packaging/uninstall.sh)
#   # Or locally:
#   sudo bash packaging/uninstall.sh
# ==============================================================================

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

# 1. Root privilege check
if [[ $EUID -ne 0 ]]; then
    error "This script must be run as root (or with sudo)."
fi

# 2. Confirmation prompt (skip if passed -y or --yes or NONINTERACTIVE=1)
AUTO_CONFIRM="${NONINTERACTIVE:-0}"
PURGE_DATA=0

for arg in "$@"; do
    case "$arg" in
        -y|--yes)
            AUTO_CONFIRM=1
            ;;
        --purge)
            PURGE_DATA=1
            ;;
    esac
done

if [[ "$AUTO_CONFIRM" -ne 1 ]]; then
    echo -e "${YELLOW}========================================================${PLAIN}"
    echo -e "${YELLOW}               3R-UI 卸载程序                          ${PLAIN}"
    echo -e "${YELLOW}========================================================${PLAIN}"
    echo "即将卸载 3R-UI 服务及其关联进程。"
    echo ""
    read -rp "确认要卸载 3R-UI 吗？[y/N]: " confirm_uninstall
    if [[ ! "$confirm_uninstall" =~ ^[Yy]$ ]]; then
        info "用户取消卸载操作。"
        exit 0
    fi

    echo ""
    read -rp "是否彻底删除数据目录与配置 (/var/lib/3r-ui, /etc/3r-ui, /var/log/3r-ui)？[y/N]: " confirm_purge
    if [[ "$confirm_purge" =~ ^[Yy]$ ]]; then
        PURGE_DATA=1
    fi
fi

# 3. Stop and disable systemd service
SERVICE_NAME="3r-ui"
SERVICE_FILE="/etc/systemd/system/3r-ui.service"

info "Stopping 3R-UI service..."
if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    systemctl stop "$SERVICE_NAME" || true
fi

if systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
    systemctl disable "$SERVICE_NAME" || true
fi

# 4. Kill remaining child processes (xray, mita/mieru) if any
info "Terminating any residual Xray / Mieru child processes..."
pkill -9 -f "/var/lib/3r-ui/xray/xray" 2>/dev/null || true
pkill -9 -f "mita" 2>/dev/null || true
pkill -9 -f "3r-ui" 2>/dev/null || true

# 5. Remove systemd service file
if [[ -f "$SERVICE_FILE" ]]; then
    info "Removing systemd service unit: $SERVICE_FILE..."
    rm -f "$SERVICE_FILE"
    systemctl daemon-reload
    systemctl reset-failed 2>/dev/null || true
fi

# 6. Remove binary
INSTALL_BIN="/usr/local/bin/3r-ui"
if [[ -f "$INSTALL_BIN" ]]; then
    info "Removing binary: $INSTALL_BIN..."
    rm -f "$INSTALL_BIN"
fi

# 7. Clean data, configs and logs if requested
CONF_DIR="/etc/3r-ui"
DATA_DIR="/var/lib/3r-ui"
LOG_DIR="/var/log/3r-ui"

if [[ "$PURGE_DATA" -eq 1 ]]; then
    info "Purging configurations, databases, and logs..."
    rm -rf "$CONF_DIR" "$DATA_DIR" "$LOG_DIR"
    success "Data directories purged."
else
    info "Data directories preserved:"
    echo "  - Configs: $CONF_DIR"
    echo "  - Data:    $DATA_DIR"
    echo "  - Logs:    $LOG_DIR"
    echo "若后续需手动彻底删除，可执行: rm -rf $CONF_DIR $DATA_DIR $LOG_DIR"
fi

echo ""
success "3R-UI 已成功从您的系统中完全卸载！"
