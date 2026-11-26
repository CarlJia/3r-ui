---
title: "feat: Add Fail2ban support for SSH and Web console protection"
date: 2026-11-26
type: feat
execution: code
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
---

# feat: Add Fail2ban support for SSH and Web console protection

## Goal Capsule

- **Objective:** 为 3r-ui 提供开箱即用的 Fail2ban 安全防护能力，有效防御外部针对 Linux 主机 SSH 端口及 3r-ui Web 控制台的暴力破解与凭据撞库攻击；在系统未安装 Fail2ban 时支持自动安装与智能配置，并杜绝运维人员因规则误封导致自身失联。
- **Means:** 
  1. 在安装脚本与后端管理中增加 Fail2ban 依赖检测与自动安装逻辑（`apt-get install -y fail2ban`）；
  2. 生成标准且安全的 jail 规则（包含 `[sshd]` 与 `[3r-ui]` 两个 jail），自动读取当前 SSH 端口、Web 面板端口与白名单 IP（`ignoreip`）避免运维失联；
  3. 改造 Web 登录认证（`/login`）逻辑，结构化输出包含真实客户端 IP 的认证失败审计日志，配合 fail2ban 过滤器（`filter.d/3r-ui.conf`）实现自动封禁；
  4. 在 Web 控制台“系统设置”中新增“安全与 Fail2ban”管理卡片，提供服务状态查看、Jail 激活情况、已封禁 IP 列表与一键解封（Unban）功能。
- **Authority Hierarchy:** Product Security & Host Availability (防止自锁与防止未授权入侵) > User Operational Ergonomics (一键安装与可视化管理) > Implementation Simplicity.
- **Stop Conditions:** 
  - 当主机不支持 systemd/apt 或无 root 权限执行 fail2ban 安装/管理时，降级提供纯 Web 应用层防爆破与清晰的错误提示，不破坏 3r-ui 核心代理服务；
  - 封禁规则永远不能阻断本地回环地址（`127.0.0.1/8`、`::1`）以及当前已配置的管理员管理白名单 IP。

---

## Product Contract

### Problem Frame
3r-ui 控制台及运行所在的主机通常直接暴露在公网 IP 上。黑客和扫描机器人长期对默认 SSH 端口（22）及 Web 登录端点发起持续的高频密码爆破。目前 3r-ui 缺乏安全防护联动，也没有自动检测和配置 Fail2ban 的功能，存在服务器被暴力破解攻破的安全隐患。同时，如果由用户手动编写 iptables/fail2ban 规则，极易因端口配置不匹配或缺少 ignoreip 而导致运维人员被封锁在 VPS 外。

### Requirements
- **R1:** 安装流程支持检测主机是否已安装 `fail2ban`；若未安装，安装脚本及管理指令应支持全自动安装依赖并初始化服务。
- **R2:** 提供针对 SSH 服务的 Fail2ban 保护配置（`[sshd]` jail），兼容 Debian/Ubuntu 环境下的日志检索（支持 systemd backend 或 `/var/log/auth.log`）。
- **R3:** 提供针对 3r-ui Web 登录端点的 Fail2ban 保护配置（`[3r-ui]` jail 与 `3r-ui.conf` filter），精准匹配密码错误登录失败事件。
- **R4:** 防自锁保障机制：初始化 Fail2ban 规则时自动解析当前正在使用的 SSH 端口、Web 面板监听端口，并将本地回环及初始管理 IP 加入 `ignoreip` 白名单。
- **R5:** 3r-ui Web 服务在管理员登录失败时，必须准确提取客户端真实 IP（兼容直连与反向代理场景），输出符合 Fail2ban filter 规范的结构化日志条目。
- **R6:** 在 3r-ui Web 控制台（系统设置）中提供 Fail2ban 状态卡片：
  - 展示 Fail2ban 服务运行状态（Active / Inactive / Not Installed）；
  - 展示 `sshd` 与 `3r-ui` 各自的启用状态、当前封禁 IP 数量与封禁列表；
  - 提供对被封禁 IP 的一键手动解封（Unban）操作；
  - 支持调整基础封禁参数（`maxretry` 失败阈值、`findtime` 观察窗口、`bantime` 封禁时长）。
- **R7:** 若用户未在主机级启用 Fail2ban 或运行在非 root 环境下，Web 端提供备用应用层内存防爆破拦截机制，确保面板本身始终具备基本防护。

### Scope Boundaries
- **In-Scope:**
  - `packaging/install.sh` 脚本增加 fail2ban 自动安装与配置注入。
  - 3r-ui 内置的 Fail2ban 管理模块与状态查询接口（通过安全包装调用 `fail2ban-client`）。
  - 3r-ui Web 登录端点（`src/web/auth.rs`）的客户端 IP 提取与防爆破日志输出。
  - 前端“系统设置”页面（`templates/settings.html`）新增 Fail2ban 管理组件与相关 API。
- **Out of Scope (Deferred to Follow-Up Work):**
  - 不涉及 Xray/Hysteria2/Mieru 单用户多 IP 并发连接限制（Client Concurrent IP Limit）的复杂日志解析。
  - 不接管主机的全局复杂 UFW / iptables 规则编排。

---

## Planning Contract

### Key Technical Decisions (KTDs)

- **KTD1: 双层联动防护架构（外部 Fail2ban 守护进程 + Web 规范日志 & 内部兜底）**
  - *Context:* Fail2ban 需要操作系统 root/CAP_NET_ADMIN 权限操作 iptables/nftables。
  - *Decision:* 主机运行标准的 `fail2ban` systemd 服务；3r-ui Web 仅负责按预设格式（例如 `[SECURITY] Web login failed for user='admin' from ip='x.x.x.x'`）输出标准系统安全日志（至 stdout/journald 或 `/var/log/3r-ui/auth.log`），由 Fail2ban 专有过滤器进行监视和封禁。3r-ui 后端通过只读/指定命令包装与 `fail2ban-client` 进行非破坏性交互。
  - *Rationale:* 避免在 3r-ui Rust 进程中直接集成或硬编码复杂的 iptables/nftables 规则操作，遵循 Linux 安全职责划分。

- **KTD2: SSH 端口动态自适应与防自锁设计**
  - *Context:* 用户 VPS 常常修改了默认的 22 端口，且粗暴的 drop 规则可能把管理员自身封禁。
  - *Decision:* 在生成 `/etc/fail2ban/jail.d/3r-ui.conf` 时：
    1. 动态检测 `/etc/ssh/sshd_config` 或 `sshd -T` 中的实际端口（默认回退 22）；
    2. 设置 `ignoreip = 127.0.0.1/8 ::1`，并允许在 Web 页面中追加管理员自定义白名单 IP（CIDR）；
    3. `[sshd]` jail 与 `[3r-ui]` jail 明确区分各自的端口与过滤行为，严禁全端口全局滥杀。

- **KTD3: 真实客户端 IP 提取**
  - *Context:* 3r-ui 可能通过内置 Pingora 反向代理访问，也可能通过直接 HTTP 访问。
  - *Decision:* 在 Axum 请求处理器中实现安全的客户端 IP 提取策略：优先检查 `ConnectInfo<SocketAddr>`，若前置存在受信任的本地面板代理（127.0.0.1），则读取 `X-Forwarded-For` 或 `X-Real-IP` 首个有效 IP。

- **KTD4: 自动安装机制设计**
  - *Context:* 用户明确要求“如果没有安装 fail2ban 需要自动安装”。
  - *Decision:* 
    1. 在 `packaging/install.sh` 脚本的 `MISSING_PKGS` 检查列表中默认纳入 `fail2ban`；
    2. 在 3r-ui Rust 核心启动或用户通过 Web 设置页点击“启用 Fail2ban”时，若系统检测到 `fail2ban-client` 不存在且当前具备 sudo/root 权限，提供异步自动安装触发或一键安装任务（类似 Xray 自动安装器与系统包管理器交互机制）。

### High-Level Technical Design

```mermaid
flowchart TD
    subgraph Client Requests
        SSH_Client["SSH 登录尝试 (Port 22/custom)"]
        Web_Client["Web 控制台登录 (Port 8443/custom)"]
    end

    subgraph Linux Host Security
        SSHD["sshd Daemon"]
        ThreeRUI["3r-ui Web Service"]
        Journald["systemd journal / auth.log"]
        F2B["Fail2ban Daemon"]
        Netfilter["iptables / nftables 防火墙"]
    end

    SSH_Client -->|尝试登录| SSHD
    Web_Client -->|POST /login| ThreeRUI

    SSHD -->|认证失败日志| Journald
    ThreeRUI -->|结构化安全日志: [SECURITY] Web login failed| Journald

    F2B -->|Monitor sshd jail| Journald
    F2B -->|Monitor 3r-ui jail| Journald

    F2B -->|触发封禁 (超出 maxretry)| Netfilter
    Netfilter -.->|拒绝恶意 IP 访问| SSH_Client
    Netfilter -.->|拒绝恶意 IP 访问| Web_Client

    subgraph 3r-ui Management
        Web_Settings["Web 管理界面 /admin/settings"]
        F2B_Service["src/security/fail2ban.rs"]
    end

    Web_Settings <-->|查看状态/被封IP/一键解封| F2B_Service
    F2B_Service <-->|fail2ban-client status/unban| F2B
```

---

## Implementation Units

### U1. 客户端真实 IP 提取与安全失败日志规范化
- **Description:** 在 `src/web/` 中实现可靠的客户端 IP 提取，并在 `src/web/auth.rs` 中当管理员凭据校验失败时，输出符合格式的告警安全日志。
- **Files Modified/Created:**
  - `src/web/mod.rs` (确保 Router 启用了 `into_make_service_with_connect_info::<SocketAddr>()`)
  - `src/web/auth.rs` (捕获客户端 IP，在 `valid == false` 时记录 `tracing::warn!(target: "security", client_ip = %ip, "3r-ui web login failed")` 或输出专用安全日志格式)
- **Test Scenarios:**
  - 模拟无效密码登录请求，验证日志准确输出对应的客户端 IP 与统一的失败关键字。
  - 模拟直连与代理层头信息（X-Forwarded-For），验证提取到的客户端 IP 正确。

### U2. Fail2ban 规则配置与安装脚本自动化支持
- **Description:** 编写 3r-ui 专属的 fail2ban 过滤器（`filter.d/3r-ui.conf`）与综合监狱配置模板（`jail.d/3r-ui.conf`），并在 `packaging/install.sh` 中实现 fail2ban 的自动安装与规则部署。
- **Files Modified/Created:**
  - `packaging/fail2ban/filter.d/3r-ui.conf` (正则表达式匹配 3r-ui 的登录失败日志)
  - `packaging/fail2ban/jail.d/3r-ui.conf` (包含 `[sshd]` 和 `[3r-ui]` 两个 jail，自动填充 SSH 端口与 Web 端口)
  - `packaging/install.sh` (在依赖检查阶段自动检查并执行 `apt-get install -y fail2ban`，并将规则复制到 `/etc/fail2ban/`，自动配置 ignoreip)
  - `packaging/uninstall.sh` (清理 3r-ui 对应的 fail2ban jail 配置)
- **Test Scenarios:**
  - 运行规则语法测试：`fail2ban-regex` 能够准确捕获 U1 生成的模拟登录失败日志。
  - 模拟无 fail2ban 环境运行 `install.sh`，确认 fail2ban 能够被自动拉取安装并启动服务。

### U3. 后端 Fail2ban 管理核心（状态检查、IP 列表与一键解封）
- **Description:** 在 Rust 后端新增 `src/security/fail2ban.rs` 模块，负责与系统的 `fail2ban-client` 进行交互。
- **Files Modified/Created:**
  - `src/security/mod.rs`
  - `src/security/fail2ban.rs` (检测 fail2ban 是否安装/运行、执行 `fail2ban-client status <jail>` 获取封禁列表、执行 `fail2ban-client set <jail> unbanip <ip>`、自动安装辅助脚本调用)
  - `src/lib.rs` (挂载 `pub mod security;`)
- **Test Scenarios:**
  - 当系统中未安装 fail2ban 时，接口安全返回 `installed: false`，不产生 panic 或服务崩溃。
  - 当 fail2ban 正常运行时，成功解析出 `sshd` 和 `3r-ui` 各自的 `Banned IP list`。
  - 调用 unban 接口，验证正确执行 `fail2ban-client set <jail> unbanip <ip>`。

### U4. 数据库存储安全设置与白名单持久化
- **Description:** 新增安全配置持久化表，支持在数据库中保存 fail2ban 启用状态、自定义封禁时长（bantime）、重试次数（maxretry）以及额外白名单 IP 列表（ignoreip）。
- **Files Modified/Created:**
  - `migrations/0010_security_settings.sql` (创建 `security_settings` 单例表)
  - `src/db/models.rs` (新增 `SecuritySettings` 结构体及读写方法)
- **Test Scenarios:**
  - 迁移脚本正常执行，初始数据默认安全填充。
  - 修改白名单 IP 或封禁时间后更新数据库，重载配置生效。

### U5. Web 控制台“安全与 Fail2ban”管理界面
- **Description:** 在 Web 端“系统设置”页面或独立安全设置选项卡中，提供可视化的 Fail2ban 状态卡片。
- **Files Modified/Created:**
  - `src/web/settings.rs` (新增获取安全状态、更新配置、一键安装、解封 IP 的 API 端点)
  - `templates/settings.html` (添加 Fail2ban 状态面板：显示安装状态、SSH/Web 监狱运行状态、被封禁 IP 表格、解除封禁按钮、白名单编辑框)
- **Test Scenarios:**
  - 页面加载时自动异步加载 Fail2ban 实时状态。
  - 点击“解除封禁”按钮，表格即时刷新并移除该 IP。
  - 若系统未安装 fail2ban，界面高亮提示并提供“一键安装并启用”按钮。

---

## Verification Contract

- **自动化测试：**
  - `cargo test --lib security`：验证 fail2ban 客户端输出解析器（包括不同 fail2ban 版本输出的 Banned IP 解析）。
  - `cargo test --test settings`：验证安全配置读取、更新与校验逻辑。
- **端到端测试与集成验证：**
  - **安装验证：** 在干净的测试环境运行 `packaging/install.sh`，确认 `fail2ban` 被自动安装，`/etc/fail2ban/jail.d/3r-ui.conf` 与 `filter.d/3r-ui.conf` 存在且配置有效。
  - **SSH 保护验证：** 执行 `fail2ban-client status sshd`，确认 jail 处于 active 状态并监控正确的端口与日志路径。
  - **Web 防爆破验证：** 连续发起 5 次错误的 Web 登录 POST 请求，确认：
    1. 3r-ui 日志输出对应的 `Web login failed` 记录；
    2. `fail2ban-client status 3r-ui` 中出现该客户端 IP；
    3. 后续 Web 请求被防火墙直接拦截（Connection Refused / Drop）；
    4. 登录 Web 控制台或通过 CLI 执行 unban 后，客户端恢复正常访问。
  - **防自锁验证：** 确认 `ignoreip` 包含回环地址及预设白名单，对白名单 IP 连续失败登录不会触发封禁。

---

## Definition of Done

- [ ] `packaging/install.sh` 能够自动检测并安装 `fail2ban`，并配置好 `sshd` 与 `3r-ui` jail。
- [ ] SSH 端口与 Web 面板端口均受到 fail2ban 保护，且白名单机制有效防止管理员失联。
- [ ] 3r-ui Web 登录失败时输出标准化安全日志，能够被 `filter.d/3r-ui.conf` 准确认证捕获。
- [ ] Web 控制台直观展示 Fail2ban 运行状态、受保护服务状态与被封禁 IP 列表，并支持一键解封。
- [ ] 所有新增代码通过 `cargo clippy --all-targets` 且无任何 warning，新增单元测试全部通过。
