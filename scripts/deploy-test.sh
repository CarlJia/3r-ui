#!/usr/bin/env bash
# ==============================================================================
# 3R-UI Fast VPS Test Deployer
#
# 专为开发测试设计的秒级热更脚本，解决频繁删打 Git tag 和等待 GitHub CI 的痛点。
# 支持在 scripts/deploy.conf 中预设 VPS_HOST、VPS_USER、VPS_PORT、VPS_PASS，
# 执行时全自动登录，无需每次手动输入 IP 与密码。
#
# 支持两种连接模式:
#   1. SSH Host 别名模式 (推荐):
#      直接使用 ~/.ssh/config 中配置的 Host (如 ssh nb-3rui)。
#      脚本自动继承 ~/.ssh/config 中的 HostName、User、Port、IdentityFile 等配置。
#   2. 传统 IP 直连模式:
#      通过 user@ip[:port] 直连，默认 user 为 root，默认端口为 22。
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

# 帮助信息
show_help() {
    cat << EOF
3R-UI 快速 VPS 测试部署脚本 (Fast Test Deployer)

用法:
  $0 [选项] [目标: host-alias | user@vps-ip[:port]] [arch]

支持模式:
  1. SSH Host 模式 (如 ssh nb-3rui):
     直接支持 ~/.ssh/config 中定义的 Host 别名，自动继承端口、用户与认证配置。
     示例:
       $0 nb-3rui
       $0 ssh nb-3rui
       $0 --host nb-3rui
       $0 nb-3rui aarch64

  2. 传统 IP 模式:
     直接通过 IP / 域名连接，默认用户 root，默认端口 22。
     示例:
       $0 root@1.2.3.4
       $0 root@1.2.3.4:2222
       $0 1.2.3.4 aarch64

选项:
  --host, -H <host>      指定 SSH Host 别名 (如 nb-3rui)
  --no-build             跳过本地编译，直接上传已有的编译产物
  -p, --port <port>      显式指定端口号 (覆盖默认值或 Host 配置)
  -a, --arch <arch>      目标架构 (x86_64 或 aarch64，默认 x86_64)
  -h, --help             显示此帮助信息

提示:
  可在 scripts/deploy.conf 中预设 VPS_HOST (支持填别名如 nb-3rui) 与相关配置，
  后续在终端直接运行 $0 即可全自动部署！
EOF
}

# 尝试从 scripts/deploy.conf 读取默认 VPS 配置
CONF_FILE="$SCRIPT_DIR/deploy.conf"
if [[ -f "$CONF_FILE" ]]; then
    # shellcheck source=/dev/null
    source "$CONF_FILE"
fi

# 参数解析
DO_BUILD=true
IS_HOST_MODE=false
CUSTOM_TARGET=""
CUSTOM_PORT=""
CUSTOM_ARCH=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        ssh|host)
            IS_HOST_MODE=true
            shift
            ;;
        --no-build)
            DO_BUILD=false
            shift
            ;;
        --host|-H)
            [[ $# -ge 2 ]] || die "缺少参数: $1 需要指定 host 别名 (如 --host nb-3rui)"
            IS_HOST_MODE=true
            CUSTOM_TARGET="$2"
            shift 2
            ;;
        -H*)
            IS_HOST_MODE=true
            CUSTOM_TARGET="${1#-H}"
            shift
            ;;
        --host=*)
            IS_HOST_MODE=true
            CUSTOM_TARGET="${1#*=}"
            shift
            ;;
        -p|--port)
            [[ $# -ge 2 ]] || die "缺少参数: $1 需要指定端口号"
            [[ "$2" =~ ^[0-9]+$ ]] || die "端口号格式无效: $2"
            CUSTOM_PORT="$2"
            shift 2
            ;;
        -p*)
            CUSTOM_PORT="${1#-p}"
            [[ -n "$CUSTOM_PORT" && "$CUSTOM_PORT" =~ ^[0-9]+$ ]] || die "端口号格式无效: $1"
            shift
            ;;
        --port=*)
            CUSTOM_PORT="${1#*=}"
            [[ -n "$CUSTOM_PORT" && "$CUSTOM_PORT" =~ ^[0-9]+$ ]] || die "端口号格式无效: $1"
            shift
            ;;
        -a|--arch)
            [[ $# -ge 2 ]] || die "缺少参数: $1 需要指定架构 (x86_64 或 aarch64)"
            CUSTOM_ARCH="$2"
            shift 2
            ;;
        -a*)
            CUSTOM_ARCH="${1#-a}"
            shift
            ;;
        --arch=*)
            CUSTOM_ARCH="${1#*=}"
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            if [[ -z "$CUSTOM_TARGET" ]]; then
                CUSTOM_TARGET="$1"
            elif [[ -z "$CUSTOM_ARCH" ]]; then
                CUSTOM_ARCH="$1"
            else
                die "未知参数: $1 (可运行 $0 -h 查看帮助)"
            fi
            shift
            ;;
    esac
done

# 目标判断与解析
VPS_TARGET="${CUSTOM_TARGET:-${SSH_HOST:-${VPS_HOST:-}}}"
if [[ -z "$VPS_TARGET" || "$VPS_TARGET" == "your-vps-ip" ]]; then
    show_help
    exit 1
fi

# 检查是否为标准 IP 格式 (IPv4 或 IPv6)
is_ip_format() {
    local target="$1"
    local host_only="${target#*@}"  # 剥离 user@
    host_only="${host_only%%:*}"    # 剥离 :port
    host_only="${host_only#[}"      # 剥离 [
    host_only="${host_only%]}"      # 剥离 ]

    # IPv4 正则校验
    if [[ "$host_only" =~ ^[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$ ]]; then
        return 0
    fi
    # IPv6 (包含至少一个冒号)
    if [[ "$host_only" == *:* ]]; then
        return 0
    fi
    return 1
}

# 判断是否启用 Host 模式
# 满足以下任意一条即视为 Host 模式:
# 1. 命令行显式指定了 --host / -H / ssh / host 前缀
# 2. 配置文件或环境变量设置了 HOST_MODE=true 或 SSH_HOST
# 3. 目标不含 @ 且非纯 IP 地址 (如 "nb-3rui")
if $IS_HOST_MODE || [[ "${HOST_MODE:-false}" == "true" ]] || [[ -n "${SSH_HOST:-}" ]]; then
    IS_HOST_MODE=true
elif ! is_ip_format "$VPS_TARGET" && [[ "$VPS_TARGET" != *@* ]]; then
    IS_HOST_MODE=true
fi

# 端口解析
PORT_SPECIFIED=false
if [[ -n "$CUSTOM_PORT" ]]; then
    VPS_PORT="$CUSTOM_PORT"
    PORT_SPECIFIED=true
elif [[ "$VPS_TARGET" == *:* ]]; then
    PORT_PART="${VPS_TARGET##*:}"
    HOST_PART="${VPS_TARGET%:*}"
    if [[ "$PORT_PART" =~ ^[0-9]+$ ]]; then
        VPS_PORT="$PORT_PART"
        VPS_TARGET="$HOST_PART"
        PORT_SPECIFIED=true
    fi
fi

if $IS_HOST_MODE; then
    # Host 模式 (如 ssh nb-3rui)
    # 若未包含 user@，直接使用 host 别名连接，由 ~/.ssh/config 指定用户与认证
    VPS_CONN="$VPS_TARGET"
    say "连接模式: SSH Host 模式 -> [${VPS_CONN}]"
else
    # 传统 IP 模式
    VPS_PORT="${VPS_PORT:-22}"
    if [[ "$VPS_TARGET" != *@* ]]; then
        VPS_USER="${VPS_USER:-root}"
        VPS_CONN="${VPS_USER}@${VPS_TARGET}"
    else
        VPS_CONN="$VPS_TARGET"
    fi
    say "连接模式: 传统 IP 模式 -> [${VPS_CONN}] (端口: ${VPS_PORT})"
fi

# 目标架构
ARCH="${CUSTOM_ARCH:-${TARGET_ARCH:-${ARCH:-x86_64}}}"
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

# SSH / SCP 基础选项
SSH_OPTS=(-o StrictHostKeyChecking=no -o ConnectTimeout=10)
SCP_OPTS=(-o StrictHostKeyChecking=no -o ConnectTimeout=10)

# 仅在非 Host 模式，或用户显式指定了端口时，才传递 -p / -P
# Host 模式下默认遵循 ~/.ssh/config 中的 Port
if ! $IS_HOST_MODE || $PORT_SPECIFIED; then
    VPS_PORT="${VPS_PORT:-22}"
    SSH_OPTS+=(-p "$VPS_PORT")
    SCP_OPTS+=(-P "$VPS_PORT")
fi

if [[ -n "${SSH_KEY:-}" && -f "${SSH_KEY/#\~/$HOME}" ]]; then
    SSH_OPTS+=(-i "${SSH_KEY/#\~/$HOME}")
    SCP_OPTS+=(-i "${SSH_KEY/#\~/$HOME}")
fi

if [[ -n "${SSH_EXTRA_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    EXTRA_ARGS_ARR=($SSH_EXTRA_ARGS)
    SSH_OPTS+=("${EXTRA_ARGS_ARR[@]}")
    SCP_OPTS+=("${EXTRA_ARGS_ARR[@]}")
fi

SSH_PASS_PREFIX=()
EFFECTIVE_PASS=""
# 若目标为配置文件中的 VPS_HOST，或环境变量有显式密码，则使用密码
if [[ -n "${VPS_PASS:-}" ]]; then
    if [[ -z "$CUSTOM_TARGET" || "$CUSTOM_TARGET" == "${VPS_HOST:-}" ]]; then
        EFFECTIVE_PASS="$VPS_PASS"
    elif ! $IS_HOST_MODE; then
        EFFECTIVE_PASS="$VPS_PASS"
    fi
fi

if [[ -n "$EFFECTIVE_PASS" ]]; then
    if command -v sshpass >/dev/null 2>&1; then
        SSH_PASS_PREFIX=(sshpass -p "$EFFECTIVE_PASS")
    else
        warn "检测到配置文件中设置了 VPS_PASS 密码，但系统未安装 sshpass 工具。"
        warn "macOS 安装方法: brew install hudochenkov/sshpass/sshpass"
        warn "更推荐方案 (无需安装任何工具且更安全):"
        if ! $IS_HOST_MODE || $PORT_SPECIFIED; then
            warn "  直接在终端运行一次公钥免密命令: ssh-copy-id -p $VPS_PORT $VPS_CONN"
        else
            warn "  直接在终端运行一次公钥免密命令: ssh-copy-id $VPS_CONN"
        fi
        warn "本次将退回由 SSH 终端交互式输入密码..."
    fi
fi

run_ssh() {
    # shellcheck disable=SC2029
    if [[ ${#SSH_PASS_PREFIX[@]} -gt 0 ]]; then
        "${SSH_PASS_PREFIX[@]}" ssh "${SSH_OPTS[@]}" "$@"
    else
        ssh "${SSH_OPTS[@]}" "$@"
    fi
}

run_scp() {
    if [[ ${#SSH_PASS_PREFIX[@]} -gt 0 ]]; then
        "${SSH_PASS_PREFIX[@]}" scp "${SCP_OPTS[@]}" "$@"
    else
        scp "${SCP_OPTS[@]}" "$@"
    fi
}

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

# 2. 上传（压缩传输）
# 静态链接的面板二进制有 20MB+，在窄带或高延迟链路上直传会非常慢
# （实测只有几 KB/s）。gzip 之后通常能压到三分之一以下。
say "压缩二进制包..."
GZ_BIN="$(mktemp -t 3r-ui-upload).gz"
cleanup_upload() { rm -f "$GZ_BIN"; }
trap cleanup_upload EXIT
# 使用 -n 忽略时间戳与文件名，保证二进制未变时每次压缩产物的字节流与哈希 100% 确定可复现
gzip -9 -n -c "$TARGET_BIN" > "$GZ_BIN"
RAW_SIZE="$(python3 -c "import os, sys; print(f'{os.path.getsize(sys.argv[1])/1024/1024:.1f}MB')" "$TARGET_BIN")"
GZ_SIZE="$(python3 -c "import os, sys; print(f'{os.path.getsize(sys.argv[1])/1024/1024:.1f}MB')" "$GZ_BIN")"
say "原始大小 ${RAW_SIZE} -> 压缩后 ${GZ_SIZE}"
say "上传压缩包到 ${VPS_CONN}..."

upload_with_progress() {
    local src="$1"
    local dest_remote="$2"
    local remote_tmp="/tmp/3r-ui.test.tmp.gz"
    local remote_meta="/tmp/3r-ui.test.meta"
    
    # 优先使用 python3 进行带断点续传、百分比、传输量和实时速度的流式上传
    if command -v python3 >/dev/null 2>&1; then
        # 1. 计算本地文件的 SHA-256 和大小
        local file_info
        file_info="$(python3 -c "
import sys, os, hashlib
p = sys.argv[1]
size = os.path.getsize(p)
with open(p, 'rb') as f:
    h = hashlib.sha256(f.read()).hexdigest()
print(f'{size} {h}')
" "$src")"
        local local_size="${file_info%% *}"
        local local_hash="${file_info##* }"

        # 2. 检查远端是否已存在部分文件及 meta 信息
        local remote_offset=0
        local check_output
        check_output="$(run_ssh "$dest_remote" "
if [ -f '$remote_meta' ] && [ -f '$remote_tmp' ]; then
    read -r m_size m_hash < '$remote_meta' 2>/dev/null || true
    if [ \"\\\$m_hash\" = '$local_hash' ]; then
        stat -c '%s' '$remote_tmp' 2>/dev/null || stat -f '%z' '$remote_tmp' 2>/dev/null || wc -c < '$remote_tmp'
    else
        rm -f '$remote_tmp' '$remote_meta'
        echo '0'
    fi
else
    echo '0'
fi
" 2>/dev/null | tr -d '\r\n' || echo "0")"

        if [[ "$check_output" =~ ^[0-9]+$ ]] && [ "$check_output" -gt 0 ]; then
            if [ "$check_output" -ge "$local_size" ]; then
                say "远端已存在完整且哈希匹配的压缩包 (${GZ_SIZE})，直接跳过上传！"
                return 0
            fi
            remote_offset="$check_output"
            local offset_mb
            offset_mb="$(python3 -c "import sys; print(f'{int(sys.argv[1])/1024/1024:.1f}MB')" "$remote_offset")"
            say "检测到未完成的上传，触发断点续传 (从 ${offset_mb} / ${GZ_SIZE} 处继续)..."
        else
            # 记录当前版本的 meta 信息用于下次续传校验
            run_ssh "$dest_remote" "echo '${local_size} ${local_hash}' > '$remote_meta'" 2>/dev/null || true
        fi

        # 3. 从 offset 开始流式传输并追加写入远端文件
        python3 -c "
import sys, os, time, signal

def on_sigint(sig, frame):
    sys.stderr.write('\n\033[0;33m[TEST-DEPLOY]\033[0m 上传已中断 (已保留断点进度，下次执行将自动续传)。\n')
    sys.stderr.flush()
    # 使用 os._exit(130) 直接终止进程，跳过 Python 运行时的 stdout flush 与清理钩子，避免 Exception ignored
    os._exit(130)

signal.signal(signal.SIGINT, on_sigint)

src_path = sys.argv[1]
offset = int(sys.argv[2])
total = os.path.getsize(src_path)
sent = offset
t0 = time.time()
last_print = 0

try:
    with open(src_path, 'rb') as f:
        if offset > 0:
            f.seek(offset)
        while True:
            chunk = f.read(65536)
            if not chunk:
                break
            try:
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()
            except (BrokenPipeError, IOError):
                os._exit(1)
            sent += len(chunk)
            now = time.time()
            if now - last_print >= 0.2 or sent == total:
                elapsed = max(now - t0, 0.001)
                # 计算当前阶段的即时传输速率
                speed = ((sent - offset) / elapsed) / 1024 / 1024
                pct = int(sent * 100 / total)
                mb_sent = sent / 1024 / 1024
                mb_total = total / 1024 / 1024
                bar_len = 20
                filled = int(bar_len * sent / total)
                bar = '=' * filled + '>' if filled < bar_len else '=' * bar_len
                bar = bar.ljust(bar_len)
                tag = '续传进度' if offset > 0 else '上传进度'
                sys.stderr.write(f'\r\033[0;34m[TEST-DEPLOY]\033[0m {tag}: [{bar}] {pct:3d}% ({mb_sent:4.1f}MB / {mb_total:4.1f}MB, {speed:4.1f} MB/s)')
                sys.stderr.flush()
                last_print = now
    sys.stderr.write('\n')
except KeyboardInterrupt:
    on_sigint(None, None)
" "$src" "$remote_offset" | run_ssh "$dest_remote" "cat >> '$remote_tmp'" || {
            local exit_code=$?
            if [ $exit_code -eq 130 ]; then
                exit 130
            fi
            die "上传中断或失败 (错误码 $exit_code)"
        }
    else
        # 没有 python3 时退回到原始 scp
        run_scp "$src" "${dest_remote}:${remote_tmp}"
    fi
}

upload_with_progress "$GZ_BIN" "${VPS_CONN}"
ok "上传成功。"

# 3. 远程热更服务
say "在 VPS 上替换二进制并重启 3r-ui 服务..."
run_ssh "${VPS_CONN}" bash -s << 'REMOTE_SCRIPT'
set -euo pipefail

# 权限校验
if [ "$(id -u)" -ne 0 ]; then
    SUDO="sudo"
else
    SUDO=""
fi

# 解压上传的压缩包
$SUDO gunzip -c /tmp/3r-ui.test.tmp.gz > /tmp/3r-ui.test.tmp
$SUDO chmod +x /tmp/3r-ui.test.tmp
rm -f /tmp/3r-ui.test.tmp.gz

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
