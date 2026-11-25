# 变更日志 (Changelog)

本项目的所有显著变更都将记录在此文件中。格式基于 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.0.0/)，并遵循 [语义化版本 2.0.0](https://semver.org/lang/zh-CN/)。

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
