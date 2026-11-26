# 变更日志 (Changelog)

本项目的所有显著变更都将记录在此文件中。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，并遵循 [语义化版本 2.0.0](https://semver.org/lang/zh-CN/)。

---

## [v0.0.3] - 2026-11-26

### ✨ 新增功能 (Features)

- **多网卡 IP（listen_ip）绑定与 CDN 节点直连地址解耦**：
  - 允许为不同入站协议（VLESS、Mieru、Hysteria 2）分别指定监听的多网卡 IP，支持多 IP 端口复用（允许不同具体 IP 绑定相同端口）。
  - 在启用全局 CDN 反代域名的场景下，将订阅分发域名与节点连接地址解耦，支持自定义直连域名与回退网卡公网 IP，防止直连流量被误路由至 CDN。
  - Web 面板入站表单支持自动探测本机网络接口列表并提供 IP 下拉选择。

### 🐛 问题修复 (Fixes)

- **修复 Xray 误渲染 Mieru 入站导致 `empty "privateKey"` 报错**：
  - `src/xray/config.rs` 在生成 Xray 配置文件时跳过 `mieru` 协议入站，并增加 `mieru` 协议防入 Xray 校验。
  - 对 VLESS 入站增加空私钥自动自愈机制：若数据库中入站私钥为空，在渲染时自动生成 X25519 密钥对并回写数据库，避免 Xray 因空私钥启动失败。
- **Mieru 守护进程从未真正启动，客户端 ping 全部超时**：
  - `src/mieru/installer.rs` 补齐 `mita` 服务端二进制的自动下载与解压。此前该模块只实现了架构映射函数，缺失二进制时会回退写入一个只会 `sleep 3600` 的空壳脚本——面板显示守护进程"在运行"，实际没有任何端口被监听，客户端只看到超时。
  - `src/mieru/process.rs` 改为通过 `MITA_CONFIG_JSON_FILE` 环境变量向 `mita run` 传入配置文件。`mita` **仅**从该变量读取配置，缺失时会以空配置启动且不绑定任何端口。
  - 移除启动前阻塞式 `mita apply config` 调用。该命令通过 RPC 与运行中的守护进程通信，守护进程尚未启动时会永久挂起，导致 `mita run` 永远不会被执行。热重载改为优先 `mita replace config`，失败时回退为重启进程。
  - 守护进程的标准输出与错误改写入 `${DATA_DIR}/mieru/mieru.log`，绑定失败、配置错误等启动异常不再被静默丢弃。
  - 自动准备 `mita` 运行所需的系统账户与 `/var/run/mita` socket 目录，并写入 tmpfiles 规则使其在重启后自动重建。
  - `src/mieru/config.rs` 仅在本机网卡上确实存在该地址时才写入 `listenIPAddress`，避免绑定他机 IP 导致守护进程启动失败。

---

## [v0.0.1] - 2026-11-25

这是 3R-UI 的首个正式版本发布，提供轻量、高效、开箱即用的多协议网络反向代理与代理核心可视化管理面板。

### ✨ 新增功能 (Features)

#### 1. 多协议入站支持
- **VLESS 协议**：
  - 支持标准 TCP 及 XHTTP 传输通道。
  - 支持 REALITY 白名单与证书伪装，自动生成公私钥对与 shortId。
- **Hysteria 2 协议**：
  - 原生集成基于 QUIC/UDP 的 Hysteria 2 协议支持。
  - 用户 UUID 统一认证，支持自签名自动证书与自定义证书。
- **Mieru 协议**：
  - 原生集成防 GFW 主动探测与协议识别的 Mieru 协议。
  - 支持多端口及端口段（Port Range，如 `2012-2022`）配置。
  - 支持传输协议自由选择（`TCP`、`UDP`、`TCP,UDP`）。
  - Mieru 独立守护进程管理，支持跨平台二进制自动下载与热重载。

#### 2. 内置反向代理 (Pingora) 与 TLS 证书自动化
- 内置基于 Cloudflare Pingora 的高性能异步反向代理。
- 支持 ACME DNS-01 验证协议，通过 Cloudflare API 自动化申请与静默续签 Let's Encrypt TLS 证书。
- 证书目录可视化查看与在线备份下载。

#### 3. 运维仪表盘与资源监控
- 实时展示系统运行状态：系统负载（Load Average）、CPU 使用量、内存占用量、硬盘使用量。
- 实时网络吞吐统计（In/Out 带宽与累计流量）。
- 控制台实时呈现 `3r-ui`、`xray` 及 `mieru` 的常驻内存（RSS）与综合开销。
- 访问记录与 Xray 核心运行日志统一查看面板。

#### 4. 用户与聚合订阅管理
- 客户端账户生命周期管理（UUID、配额限制、过期时间控制、状态开关）。
- 聚合订阅端点（`/sub/:token`），单订阅链接自动聚合生成 VLESS、Hysteria 2 与 Mieru 节点配置。
- 节点分享链接及二维码一键复制导出。

#### 5. 一键部署与全生命周期运维脚本
- **VPS 一键在线安装脚本**（`packaging/install.sh`）：支持直接从 Release 下载最新二进制，或指定本地上传的产物完成自动安装与自启注册。
- **VPS 一键完全卸载脚本**（`packaging/uninstall.sh`）：自动停止服务、清理残留子进程并选择性清空配置与数据。
- **本地批量远程部署工具**（`scripts/deploy.sh` 与 `scripts/deploy.conf.example`）：支持本地一键通过 SSH 部署多台远程 VPS 并自动开启 BBR 加速。

#### 6. CI/CD 自动化构建
- 完整的 GitHub Actions CI 测试检查流与多架构交叉编译矩阵（Linux `x86_64-unknown-linux-musl`、Linux `aarch64-unknown-linux-musl`、macOS `aarch64-apple-darwin`）。
- 打 tag 自动触发 Release 构建、打包 `.tar.gz` 与计算 `sha256` 校验和发布。

---

### 📄 许可证与合规
- 添加 GNU General Public License v3.0 (GPL-3.0-or-later) 开源许可证。
- 明确添加使用免责声明条款。
