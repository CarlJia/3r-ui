---
title: Cloudflare Tunnels Ingress for 3R-UI Web Panel - Plan
type: feat
date: 2026-11-27
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
execution: code
---

## Goal Capsule

- Objective: 为 3R-UI 引入内建的 Cloudflare Tunnels (`cloudflared`) 守护与服务管理能力，使部署在无独立公网 IP、复杂 NAT 或家庭宽带环境下的服务器，无需开放任何入站端口或申请本地 SSL 证书，即可通过 Cloudflare Anycast 网络安全暴露并访问 Web 控制台与订阅接口。
- Means: 在面板内集成轻量级的 `cloudflared` 进程守护监督器 (Supervisor)，提供基于 Cloudflare Tunnel Token 的远程管理模式支持；在 Web 控制台“系统设置”提供状态监控与启停开关；自动将穿透目标对准本地面板 HTTP 监听端口 (`127.0.0.1:2053`)，与公网场景下的 Pingora 反向代理形成清晰分工互补。
- Authority Hierarchy: 用户确认的业务需求 > 产品契约 (Product Contract) > 技术决策 (Planning Contract) > 实现单元 (Implementation Units)。
- Execution Profile: code
- Stop Conditions: 完成 SQLite 数据表迁移、`cloudflared` 守护进程生命周期实现、Web 设置页管理组件与 REST API、状态及日志诊断查询；通过本地测试套件、`cargo fmt` 与 `cargo clippy --all-targets -- -D warnings` 零告警。

---

## Product Contract

### Summary
本方案为 3R-UI 面板提供针对 NAT VPS 与内网机器的 Cloudflare Tunnel (`cloudflared`) 原生守护支持。针对缺乏公网独立 IPv4/IPv6 或受制于运营商封锁 Web 端口的部署场景，管理员只需在 Cloudflare Zero Trust 控制台创建一个 Tunnel，并将其 Token 填入面板“系统设置”，3R-UI 即可自动管理与拉起 `cloudflared` 守护进程，建立出站隧道直连 Cloudflare 边缘网络。外部 HTTPS 流量在 Cloudflare 边缘终结，通过加密隧道回源至面板内部 HTTP 端口 (`127.0.0.1:2053`)。该模式与面向公网 VPS 的 Pingora 反代形成清晰互补，彻底解决内网机器访问面板与订阅分发的难题。

### Problem Frame
在 NAT VPS、家宽群晖/PVE 虚拟机或受严格防火墙管控的服务器上部署 3R-UI 时，用户面临以下痛点：
1. **无公网 IP 导致面板失联**：机器仅有内网 IP 或共享公网 IPv4，无法通过域名或常规端口直连 3R-UI 控制台；
2. **入站端口开放风险与门槛**：即使有公网 IP，直接暴露 8443 等 Web 端口也常面临运营商封锁或恶意 IP 扫描嗅探；
3. **证书申请繁琐**：没有独立公网 IP 或 80/443 端口时，申请 Let's Encrypt 证书容易失败；
4. **外部工具割裂**：管理员若手动在系统里装 `cloudflared` 并用 systemd 管理，面板内部无法感知其运行状态、连接延迟与故障日志，体验支离破碎。

### Key Decisions
- KD1. **远程纳管模式优先 (Token 模式)**：采用 Cloudflare 官方推荐的 Remotely-Managed Tunnel 模式（执行 `cloudflared tunnel run --token <TOKEN>`）。用户在 Cloudflare 网页端配置域名路由与 Ingress，面板仅需保存 Token 并接管守护生命周期，免除本地配置复杂的 `config.yml` 和 credentials json。Governs R1, R2.
- KD2. **直接回源本地 Axum 面板服务**：Cloudflare Tunnel 的回源目标直接指向本地面板服务 (`http://127.0.0.1:2053`)。在纯内网/NAT 场景下，用户无需开启 Pingora 反代或申请证书；与 Pingora 形成二选一的分流架构。Governs R3.
- KD3. **非阻塞健壮的进程守护器**：借鉴项目中 `MieruSupervisor` 的成功模式，实现独立的 `CloudflaredSupervisor`，支持状态检查、日志行环形缓冲过滤、自动崩溃重试（带退避）以及优雅停机。Governs R4, R5.
- KD4. **运行状态感知与友好引导**：在 UI 上实时呈现 `cloudflared` 二进制是否已安装、当前运行状态 (Stopped / Running / Error)、进程 PID 及最近连接日志；如果系统未找到 `cloudflared` 二进制，明确给出快速安装指导，不静默失败。Governs R6, R7.

### Requirements

#### 存储与设置模型
- R1. 系统必须在 SQLite 数据库中持久化存储 Cloudflare Tunnel 配置，包括：`enabled`（是否启用）、`tunnel_token`（Cloudflare 生成的 Base64 Token）、`custom_path`（自定义 cloudflared 二进制路径，可选）、`last_status`（最后运行状态摘要）及 `updated_at`。
- R2. 系统必须提供安全的读写 API，对输入的 Token 进行去首尾空格、校验基本格式等预处理。

#### 进程守护与生命周期
- R3. 系统必须支持通过 `cloudflared tunnel run --token <TOKEN>` 启动子进程，回源直连面板绑定的本地监听端口（默认 `127.0.0.1:2053`）。
- R4. 守护进程在面板启动时若配置为 `enabled = 1` 且 Token 非空，应自动拉起；面板退出时（Ctrl-C 或服务停止）必须向子进程发送 `SIGTERM` / `SIGKILL` 确保无残留孤儿进程。
- R5. 守护进程必须监控子进程退出状态；若意外退出需记录错误原因并在有退避延迟的前提下安全重启，避免疯狂死循环刷屏。

#### Web 界面与管理接口
- R6. 在“系统设置”页面提供独立的“Cloudflare Tunnel (穿透访问)”卡片，包含：
  - 功能开关（启用/禁用）；
  - Tunnel Token 输入框（密码型，支持显示/隐藏切换）；
  - 二进制就绪状态（已安装/未安装，未安装时提供快速安装命令提示）；
  - 运行状态徽章（运行中、已停止、异常退出）与 PID 呈现；
  - 实时操作按钮（“保存并应用”、“立即重启”、“停止”）。
- R7. 提供实时的运行诊断查询接口，允许前端拉取 `cloudflared` 最近的运行日志输出（最后 50 行），便于排查连接凭据无效或无法连接 Cloudflare 边缘的错误。

---

## Planning Contract

### Key Technical Decisions
- KTD1. **守护进程设计**：
  新建 `src/tunnel/mod.rs` 与 `src/tunnel/supervisor.rs`，定义 `CloudflaredSupervisor` 结构体及 `TunnelCommand` 枚举（`Reload`, `Restart`, `Stop`）。通过 `tokio::process::Command` 启动 `cloudflared`，将其 `stderr` 输出重定向至异步异步解析管道，维护最近 100 行内存环形缓冲区 (`VecDeque<String>`) 用于前端诊断。
- KTD2. **与现有服务关系**：
  在 `src/lib.rs` 中引入 `tunnel::CloudflaredSupervisor`，在主服务启动流程中初始化并分配 `mpsc::channel`。与 `ProxySupervisor`（Pingora）解耦，两者互不强求，但可在 Web 界面给出互补建议。
- KTD3. **二进制自动嗅探与路径解析**：
  提供 `find_cloudflared_bin()` 工具函数，依次检索：用户设置的自定义路径 -> `/usr/local/bin/cloudflared` -> `/usr/bin/cloudflared` -> 系统 `$PATH` 探测。避免因 Linux 发行版安装路径差异导致找不到二进制。
- KTD4. **安全与 Token 保护**：
  在 API 返回设置信息时，对 `tunnel_token` 进行脱敏处理（仅返回掩码或提供独立的写入接口），防止通过普通查看接口泄露高敏感 Tunnel 凭证。

### High-Level Technical Design

```
+-----------------------------------------------------------------------------------+
|                              Cloudflare Edge Network                              |
+-----------------------------------------------------------------------------------+
                                         ^
                                         | Outbound HTTP/2 or QUIC (TLS)
                                         v
+-----------------------------------------------------------------------------------+
| 3R-UI Server (NAT / No Public IP Host)                                           |
|                                                                                   |
|  +-----------------------------------------------------------------------------+  |
|  | cloudflared Daemon Process                                                  |  |
|  | Command: cloudflared tunnel run --token <TOKEN>                             |  |
|  +-----------------------------------------------------------------------------+  |
|                                        |                                          |
|                                        | Local Reverse HTTP Proxy                 |
|                                        v                                          |
|  +-----------------------------------------------------------------------------+  |
|  | 3R-UI Axum Web Server (127.0.0.1:2053)                                     |  |
|  | - /login, /api/*, /sub/*                                                    |  |
|  +-----------------------------------------------------------------------------+  |
|                                        ^                                          |
|                                        | Lifecycle Channel (Reload / Stop)        |
|  +-------------------------------------+---------------------------------------+  |
|  | CloudflaredSupervisor (tokio actor in src/tunnel/supervisor.rs)             |  |
|  | - SQLite Config: tunnel_settings                                           |  |
|  | - Child Process Management & PID Tracking                                   |  |
|  | - Ring Buffer Stderr Log Capture (Diagnostics)                              |  |
|  +-----------------------------------------------------------------------------+  |
+-----------------------------------------------------------------------------------+
```

### State Machine of CloudflaredSupervisor

```
  +--------------+
  |   Disabled   |<-----------------------------------+
  +--------------+                                    |
         | (User enables with Token)                  |
         v                                            | Stop / Disabled
  +--------------+                                    |
  |   Starting   |                                    |
  +--------------+                                    |
         | (Spawn cloudflared)                        |
         +--------------------+                       |
         |                    |                       |
         v (Success)          v (Binary not found/err)|
  +--------------+     +--------------+               |
  |   Running    |     |    Error     |---------------+
  +--------------+     +--------------+
         |                    ^
         | (Process Crashed)  | (Retry Limit Reached)
         +--------------------+
```

---

## Implementation Units

### U1. Database Migration for Tunnel Settings
- Files: `migrations/0014_tunnel_settings.sql`, `src/db/models.rs`
- Description: 创建 `tunnel_settings` 单例配置表，包含字段：`enabled` (INTEGER), `tunnel_token` (TEXT), `custom_path` (TEXT), `last_status` (TEXT), `updated_at` (INTEGER)。在 `src/db/models.rs` 中定义对应的 `TunnelSettings` 结构体及默认值。
- Test Scenarios:
  1. 数据库升级后 `tunnel_settings` 表成功创建且初始行为单例默认记录。
  2. 查询与更新 `TunnelSettings` 正确持久化字段值。

### U2. Cloudflared Supervisor & Process Lifecycle
- Files: `src/tunnel/mod.rs`, `src/tunnel/supervisor.rs`, `src/tunnel/process.rs`, `src/lib.rs`
- Description: 编写 `CloudflaredSupervisor`，负责执行 `cloudflared` 二进制探测、子进程拉起、`SIGTERM` 停机、意外退出重试机制，以及收集 stderr 最新 100 行日志至环形缓冲队列。将其生命周期整合进 `src/lib.rs`。
- Test Scenarios:
  1. 当配置未启用时，supervisor 保持空闲并不拉起任何进程。
  2. 模拟伪造二进制或正常脚本启动，supervisor 成功追踪子进程并在发送 `Stop` 命令后干净杀死子进程。
  3. 日志收集器正确截取输出行且不超过设定的最大条数。

### U3. Web API Endpoints for Cloudflare Tunnel
- Files: `src/web/settings.rs`, `src/web/mod.rs`
- Description: 在 Web 设置模块增加 Tunnel 相关 API：
  - `GET /api/settings/tunnel`: 获取当前 Tunnel 配置（Token 掩码化）、当前运行状态、二进制是否安装、最近日志。
  - `POST /api/settings/tunnel`: 更新 Tunnel 配置并通知 Supervisor 热重载。
  - `POST /api/settings/tunnel/action`: 手动触发 `restart` 或 `stop` 动作。
- Test Scenarios:
  1. 管理员请求获取 Tunnel 设置时，返回正确的运行状态和掩码 Token。
  2. 保存有效配置后，触发 Supervisor reload 并返回成功。
  3. 未登录或非法参数时拒绝请求并返回清晰错误。

### U4. Web UI Settings Card for Cloudflare Tunnel
- Files: `templates/settings.html`
- Description: 在“系统设置”页面增加“Cloudflare Tunnel 内网穿透”卡片，包含开关切换、Token 输入与显隐按钮、运行状态指示灯 (Running/Stopped/Error)、二进制状态提示（未安装时显示一键安装命令 `curl -L ...`）、实时诊断日志折叠抽屉。
- Test Scenarios:
  1. 前端渲染设置页面时正确定位展示 Tunnel 卡片。
  2. 点击保存或手动启停时，前端发送正确 JSON 并展示 Toast 提示与状态更新。

---

## Verification Contract

### Test Commands
1. 单元与集成测试：
   ```bash
   cargo test tunnel
   cargo test settings
   ```
2. 代码质量检查：
   ```bash
   cargo fmt --check
   cargo clippy --all-targets -- -D warnings
   ```
3. 端到端本地运行验证：
   ```bash
   cargo run --bin 3r-ui -- --data-dir /tmp/test-3r-ui
   ```

---

## Definition of Done

- [ ] SQLite 迁移表 `0014_tunnel_settings.sql` 正常应用无报错。
- [ ] `CloudflaredSupervisor` 完整处理 `Reload`、`Restart`、`Stop` 信号与子进程优雅停机。
- [ ] 系统能自动检测到宿主机 `cloudflared` 二进制或提供指引。
- [ ] “系统设置”界面成功呈现 Cloudflare Tunnel 配置卡片与状态监控。
- [ ] 自动化测试全部通过，Clippy 零警告。
