#!/usr/bin/env bash
# ==============================================================================
# 3R-UI Fast VPS Test Deployer
#
# 专为开发测试设计的秒级热更脚本，解决频繁删打 Git tag 和等待 GitHub CI 的痛点。
#
# 用法:
#   ./scripts/deploy-test.sh [root@vps-ip] [x86_64|aarch64]
#
# 示例:
#   ./scripts/deploy-test.sh root@1.2.3.4
#   ./scripts/deploy-test.sh root@1.2.3.4 aarch64
#   ./scripts/deploy-test.sh --no-build root@1.2.3.4  # 跳过编译直接上传已有产物
# ==============================================================================

set -euo pipefail

# 颜色输出
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
PLAIN='\033[0m'

say() { echo -e "${BLUE}[TEST-DEPLOY]${PLAIN} $*"; }
ok()  { echo -e "${GREEN}[OK]${PLAIN} $*"; }
warn(){ echo -e "${YELLOW}[WARN]${PLAIN} $*"; }
die() { echo -e "${RED}[ERROR]${PLAIN} $*" >&2; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# 尝试从 scripts/deploy.conf 读取默认 VPS 配置
CONF_FILE="$SCRIPT_DIR/deploy.conf"
if [[ -f "$CONF_FILE" ]]; then
    # shellcheck source=/dev/null
    source "$CONF_FILE"
fi

DO_BUILD=true
if [[ "${1:-}" == "--no-build" ]]; then
    DO_BUILD=false
    shift
fi

# 目标 VPS 连接信息
VPS_TARGET="${1:-${VPS_HOST:-}}"
if [[ -z "$VPS_TARGET" || "$VPS_TARGET" == "your-vps-ip" ]]; then
    echo "使用方式: $0 [选项] <user@vps-ip[:port]> [arch]"
    echo ""
    echo "示例:"
    echo "  $0 root@1.2.3.4"
    echo "  $0 root@1.2.3.4 aarch64"
    echo "  $0 --no-build root@1.2.3.4"
    echo ""
    echo "提示: 也可在 scripts/deploy.conf 中预设 VPS_HOST，后续直接运行 $0 即可。"
    exit 1
fi

# 解析 user, host, port
VPS_PORT="${VPS_PORT:-22}"
if [[ "$VPS_TARGET" == *:* ]]; then
    # 提取端口
    PORT_PART="${VPS_TARGET##*:}"
    HOST_PART="${VPS_TARGET%:*}"
    if [[ "$PORT_PART" =~ ^[0-9]+$ ]]; then
        VPS_PORT="$PORT_PART"
        VPS_TARGET="$HOST_PART"
    fi
fi

if [[ "$VPS_TARGET" != *@* ]]; then
    VPS_USER="${VPS_USER:-root}"
    VPS_CONN="${VPS_USER}@${VPS_TARGET}"
else
    VPS_CONN="$VPS_TARGET"
fi

# 目标架构
ARCH="${2:-x86_64}"
case "$ARCH" in
    x86_64|amd64)
        TARGET_TRIPLE="x86_64-unknown-linux-musl"
        ;;
    aarch64|arm64)
        TARGET_TRIPLE="aarch64-unknown-linux-musl"
        ;;
    *)
        die "不支持的架构: $ARCH (支持 x86_64 或 aarch64)"
        ;;
esac

TARGET_BIN="$WORKSPACE_ROOT/target/${TARGET_TRIPLE}/release/3r-ui"

# SSH 选项
SSH_OPTS=(-p "$VPS_PORT" -o StrictHostKeyChecking=no -o ConnectTimeout=10)
SCP_OPTS=(-P "$VPS_PORT" -o StrictHostKeyChecking=no -o ConnectTimeout=10)
if [[ -n "${SSH_KEY:-}" && -f "${SSH_KEY/#\~/$HOME}" ]]; then
    SSH_OPTS+=(-i "${SSH_KEY/#\~/$HOME}")
    SCP_OPTS+=(-i "${SSH_KEY/#\~/$HOME}")
fi

cd "$WORKSPACE_ROOT"

# 1. 编译
if $DO_BUILD; then
    say "开始本地编译 -> 目标架构: ${TARGET_TRIPLE}..."
    
    # 优先检测 cross (需 docker/podman)
    CONTAINER_ENGINE=""
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        CONTAINER_ENGINE="docker"
    elif command -v podman >/dev/null 2>&1 && podman info >/dev/null 2>&1; then
        CONTAINER_ENGINE="podman"
    fi

    if command -v cross >/dev/null 2>&1 && [[ -n "$CONTAINER_ENGINE" ]]; then
        say "检测到 ${CONTAINER_ENGINE}，使用 cross 进行容器化交叉编译..."
        CROSS_CONTAINER_ENGINE="$CONTAINER_ENGINE" cross build --release --target "${TARGET_TRIPLE}"
    elif command -v cargo-zigbuild >/dev/null 2>&1; then
        say "使用 cargo-zigbuild 进行交叉编译..."
        cargo zigbuild --release --target "${TARGET_TRIPLE}"
    elif [[ "$(uname -s)" == "Linux" ]]; then
        say "当前为 Linux 环境，直接使用 cargo build..."
        cargo build --release --target "${TARGET_TRIPLE}"
    else
        die "未能找到可用的交叉编译工具。\n请确保安装了以下其中一种环境：\n1. Docker / Podman (配合 cross)\n2. cargo-zigbuild (配合 zig: brew install zig cargo-zigbuild)"
    fi

    [[ -f "$TARGET_BIN" ]] || die "编译产物不存在: $TARGET_BIN"
    ok "编译完成: $TARGET_BIN"
else
    say "跳过编译步骤 (--no-build)，使用现有产物: $TARGET_BIN"
    [[ -f "$TARGET_BIN" ]] || die "本地未找到编译产物: $TARGET_BIN，请先执行编译！"
fi

# 2. 上传
say "上传二进制包到 ${VPS_CONN}..."
scp "${SCP_OPTS[@]}" "$TARGET_BIN" "${VPS_CONN}:/tmp/3r-ui.test.tmp"
ok "上传成功。"

# 3. 远程热更服务
say "在 VPS 上替换二进制并重启 3r-ui 服务..."
ssh "${SSH_OPTS[@]}" "${VPS_CONN}" bash -s << 'REMOTE_SCRIPT'
set -euo pipefail

# 权限校验
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
else
    SUDO=""
fi

$SUDO chmod +x /tmp/3r-ui.test.tmp

# 停止服务并备份当前版本
if systemctl is-active --quiet 3r-ui 2>/dev/null; then
    echo "-> 停止 3r-ui 服务..."
    $SUDO systemctl stop 3r-ui
fi

if [ -f /usr/local/bin/3r-ui ]; then
    $SUDO cp -f /usr/local/bin/3r-ui /usr/local/bin/3r-ui.bak
fi

# 替换
$SUDO mv -f /tmp/3r-ui.test.tmp /usr/local/bin/3r-ui

# 启动服务并检查
echo "-> 启动 3r-ui 服务..."
$SUDO systemctl start 3r-ui

echo "-> 服务运行状态:"
$SUDO systemctl status 3r-ui --no-pager
REMOTE_SCRIPT

ok "测试包更新并部署完成！🎉"
