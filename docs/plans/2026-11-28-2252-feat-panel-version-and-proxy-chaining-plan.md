---
title: "feat: Add Panel Version Badges and Proxy Chaining (Outbound Relay) Support"
date: 2026-11-28
author: ce-plan
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
execution: code
---

# feat: Add Panel Version Badges and Proxy Chaining (Outbound Relay) Support

## Goal Capsule

- **Objective:** 提供 3R-UI 面板自身版本可视性（在管理后台侧边栏与系统设置页中展示），并为 Xray-core 提供链式出站代理（Outbound Proxy Chaining）配置支持，使入站流量可经由指定的前置/落地节点转发。
- **Means:** 
  1. 在 Askama 模板全局引入或在相关 handler 中传递 `env!("CARGO_PKG_VERSION")`，在 `sidebar_nav.html` 品牌/底部区域及 `settings.html` 系统信息卡片中明确标识「3R-UI 面板版本 v{version}」，与 `xray_version` 区分开；
  2. 新增数据库迁移与模型支持出站代理（`outbounds` 表），支持 Socks5、HTTP、Shadowsocks、VMess、VLESS 等常见落地出站协议；在入站规则（`inbounds`）中增加 `outbound_id`（或 `outbound_tag`）字段；
  3. 在 `src/xray/config.rs` 中将配置的自定义出站渲染至 Xray `outbounds` 列表中，并在路由规则（`routing.rules`）或入站的 `proxySettings` 中建立链式转发绑定；
  4. 提供出站节点管理界面（增删改查/测试连通性），并在入站编辑页允许选择出站策略（Direct 默认直连 / 指定链式代理出站）。
- **Authority Hierarchy:** Product Contract > Planning Contract > Implementation Units.
- **Stop Conditions:** 面板版本在页面明确显示且与 `Cargo.toml` 同步；入站可绑定自定义出站并正确渲染至 Xray `config.json`；单元与端到端测试覆盖通过；Clippy 与 fmt 零告警通过。

---

## Product Contract

### Problem Frame
目前 3R-UI 面板仅在「系统设置」和「控制台」中展示了底层的 Xray-core 二进制版本，用户和管理员无法直观获知当前面板运行的 3R-UI 程序版本，影响故障排查与升级感知。此外，目前 Xray-core 生成的配置中，所有的业务入站流量一律无差别路由至默认的 `direct`（freedom）出站直连，无法实现国内中转机向境外落地机进行流量接力转发（即链式代理 / 前置代理 / 出站中继）。

### Requirements

- **R1: 面板版本标识展示**
  - **R1.1:** 在后台侧边栏 `sidebar_nav.html`（Brand 旁或侧边栏底部操作区）增加 3R-UI 面板自身版本展示（如 `v0.1.0` 或 `3R-UI v0.1.0`）。
  - **R1.2:** 在系统设置页 `settings.html` 的「核心信息」卡片中新增「3R-UI 面板版本」数据项，明确与「xray 版本」并列区分。
  - **R1.3:** 版本号统一源自编译期 `env!("CARGO_PKG_VERSION")`，无需手动在数据库中硬编码。

- **R2: 出站代理节点管理 (Outbounds Management)**
  - **R2.1:** 支持添加、编辑、删除、查看自定义出站代理配置，持久化保存于 SQLite 数据库 `outbounds` 表中。
  - **R2.2:** 支持主流出站协议：`socks`、`http`、`shadowsocks`、`vmess`、`vless`。
  - **R2.3:** 字段包括：`tag`（唯一标识，如 `out-hk-01`）、`name`（展示名称）、`protocol`、`server_addr`、`server_port`、`settings_json`（包含特定协议的认证信息如 uuid、password、method、security 等）。
  - **R2.4:** 支持启用/停用开关（`status`: active / disabled）。

- **R3: 入站链式代理绑定与 Xray 配置渲染**
  - **R3.1:** 入站规则表 `inbounds` 增加可选的 `outbound_id`（关联 `outbounds.id`）。
  - **R3.2:** 在入站添加/编辑页面（`inbound_form.html`）提供「出站路由 / 链式代理」选择下拉框（默认「直连 (Direct)」，可下拉选择已启用的自定义出站节点）。
  - **R3.3:** Xray 配置渲染器（`src/xray/config.rs`）在渲染 `outbounds` 时，将活跃的自定义出站转化为 Xray 标准 outbound 对象（注入 tag、protocol、settings 等）。
  - **R3.4:** 路由规则（`routing.rules`）针对指定了出站的 inbound，渲染优先级高于默认 direct 的路由规则：`{ "type": "field", "inboundTag": [inbound_tag], "outboundTag": target_outbound_tag }`。

### Scope Boundaries

- **In Scope:**
  - 面板版本在侧边栏和系统设置中的展示。
  - `outbounds` 数据表定义与 CRUD API 及后台配置界面。
  - 入站规则支持单选出站代理实现链式转发。
  - Xray `config.json` 的 `outbounds` 和 `routing.rules` 正确渲染与重启生效。
- **Out of Scope (Deferred):**
  - Mieru 协议的链式代理（Mieru 为独立外部守护进程，由独立客户端管理）。
  - 多跳混合链（例如 A -> B -> C 三跳链，本期仅支持 Inbound -> Outbound 单级中继）。
  - 出站节点的实时测速与自动故障切换（Balancer / Fallback Group），本期为指定确定性静态路由。

---

## Planning Contract

### Key Technical Decisions (KTDs)

- **KTD1: 版本号注入机制**
  - **Decision:** 在 `src/web/mod.rs` 或特定模板视图 struct 中，直接使用 `env!("CARGO_PKG_VERSION")` 或常量 `APP_VERSION: &str = env!("CARGO_PKG_VERSION")` 注入模板字段。
  - **Rationale:** 简单高效，零运行时开销，与 `Cargo.toml` 天然同步。

- **KTD2: 链式代理的 Xray 实现策略 (Routing Rule vs ProxySettings)**
  - **Decision:** 采用 Xray 的 **Routing Rules 策略**（`inboundTag -> outboundTag`）结合 `outbounds` 定义，而非入站自身的 `proxySettings`。
  - **Rationale:** Xray 中 `proxySettings` 主要用于指定单层前置代理，而 `routing.rules` 中 `{ "type": "field", "inboundTag": ["inbound-N"], "outboundTag": "outbound-M" }` 具备清晰的流量隔离语义，与现有 API 路由规则机制高度一致，可维护性更高且调试便捷。

- **KTD3: 自定义出站协议模型结构**
  - **Decision:** 数据表设计使用基础规范字段（`id`, `tag`, `name`, `protocol`, `address`, `port`）+ `settings_json` 弹性配置字段。
  - **Rationale:** 不同代理协议（如 VMess/VLESS 的 uuid 与 tls，Shadowsocks 的 method 与 password，Socks/HTTP 的 user/pass）参数差异大。采用 `settings_json` 既保证了对多样化协议的快速兼容与未来扩展，又避免了大量宽表空字段。

---

## High-Level Technical Design

### Traffic Relay Architecture (Flow Diagram)

```mermaid
flowchart LR
    Client([客户端 Client]) -->|VLESS/Trojan/Shadowsocks| Inbound[3R-UI 入站 Inbound]
    subgraph 3R-UI / Xray-core
        Inbound --> Router{Xray Routing}
        Router -->|默认入站 (Direct)| FreedomOut[Freedom 直连出站]
        Router -->|绑定链式出站 (Relay)| ProxyOut[自定义 Outbound\nSocks5 / VMess / Shadowsocks]
    end
    FreedomOut --> TargetServer([目标网站 Target Server])
    ProxyOut -->|加密二级代理| RemoteVPS([落地中继节点 Remote VPS])
    RemoteVPS --> TargetServer
```

---

## Implementation Units

### U1. 面板版本标识展示 (UI & Handlers)
- **Goal:** 在后台侧边栏和系统设置页面清晰呈现 3R-UI 面板自身版本。
- **Requirements:** Covers R1.1, R1.2, R1.3.
- **Dependencies:** None.
- **Files:**
  - `src/web/settings.rs`
  - `src/web/mod.rs`
  - `templates/sidebar_nav.html`
  - `templates/sidebar_style.html`
  - `templates/settings.html`
  - `tests/settings.rs`
- **Approach:**
  1. 在 `templates/sidebar_nav.html` 中，在 `.sidebar-footer` 或 brand 下方加入面板版本信息小徽章（如 `<span class="version-badge">v{{ app_version }}</span>`），通过 CSS 设置微弱对比度与紧凑间距。
  2. 在 `src/web/settings.rs` 的 `SettingsTemplate` 中增加 `app_version: &'static str = env!("CARGO_PKG_VERSION")`。
  3. 在 `templates/settings.html` 的核心信息中添加 `<dt>3R-UI 版本</dt><dd>v{{ app_version }}</dd>`。
  4. 为其它使用了 `sidebar_nav.html` 的 Askama 模板（通过共同的 context 或在相应 Template struct 中补充 `app_version`）传递版本变量，或在模板内定义/使用静态方法。
- **Test Scenarios:**
  - 访问 `/admin/settings`，响应 HTML 中包含 `3R-UI 版本` 以及当前 `env!("CARGO_PKG_VERSION")` 的版本号字串。
  - 侧边栏渲染正确，版本号在浅色/深色主题下布局无破坏。

---

### U2. 数据库迁移与 Outbound 数据模型
- **Goal:** 创建 `outbounds` 表并在 `inbounds` 表中新增 `outbound_id` 关联外键。
- **Requirements:** Covers R2.1, R2.2, R2.3, R3.1.
- **Dependencies:** None.
- **Files:**
  - `migrations/0016_outbound_proxy.sql`
  - `src/db/models.rs`
  - `src/db/outbound.rs`
  - `src/db/mod.rs`
  - `tests/db_test.rs` (或新建对应集成测试)
- **Approach:**
  1. 编写 SQL 迁移 `migrations/0016_outbound_proxy.sql`：
     - 新建 `outbounds` 表（`id`, `tag`, `name`, `protocol`, `address`, `port`, `settings_json`, `status`, `created_at`, `updated_at`）。
     - 修改 `inbounds` 表增加 `outbound_id INTEGER REFERENCES outbounds(id) ON DELETE SET NULL`。
  2. 在 `src/db/models.rs` 中增加 `Outbound` 结构体及 `Inbound` 结构体字段 `outbound_id: Option<i64>`。
  3. 新增 `src/db/outbound.rs`，实现基本的数据访问函数：`list_outbounds`、`get_outbound`、`create_outbound`、`update_outbound`、`delete_outbound`。
- **Test Scenarios:**
  - 执行 `sqlx::migrate!()` 迁移成功，能正常进行 `outbounds` 的插入、查询、更新和物理/逻辑删除。
  - 当删除某个 outbound 时，关联的 inbound 其 `outbound_id` 自动置为 `NULL`。

---

### U3. Xray 配置生成器扩展 (Outbound & Routing Rules)
- **Goal:** 在 `src/xray/config.rs` 中读取活跃的出站配置与入站的出站绑定，正确生成 Xray JSON。
- **Requirements:** Covers R3.3, R3.4.
- **Dependencies:** U2.
- **Files:**
  - `src/xray/config.rs`
  - `tests/inbound.rs`
- **Approach:**
  1. 在 `render()` 函数中，从数据库加载所有 `status = 'active'` 的 outbounds。
  2. 扩展 `render_outbounds(outbounds)` 函数，将自定义 outbounds 序列化为 Xray 协议规范（例如 Socks、Shadowsocks、VMess、VLESS 等的 settings 结构），追加在系统内建的 `direct` 和 `blocked` 列表中。
  3. 遍历用户 inbounds，若 `inbound.outbound_id` 有效且指向一个活跃的 outbound，则在 `rules` 中追加一条路由规则：
     `json!({ "type": "field", "inboundTag": [format!("inbound-{}", inbound.id)], "outboundTag": target_outbound.tag })`。
  4. 确保 API 路由规则优先级仍然最靠前，其次是针对各入站的出站路由规则，最后是广告屏蔽与直连 catch-all 规则。
- **Test Scenarios:**
  - 构造包含链式代理出站的测试数据库，调用 `render()`，断言生成的 JSON 中 `outbounds` 包含自定义 tag，`routing.rules` 中包含对应的 `inboundTag -> outboundTag` 映射。
  - 构造未指定出站的常规 inbound，断言其未生成额外的特殊路由规则，默认走 direct。

---

### U4. 出站节点管理与入站编辑界面交互 (Web UI)
- **Goal:** 在 Web 后台增加出站节点管理功能，并在入站配置中支持选择出站代理。
- **Requirements:** Covers R2.1, R2.4, R3.2.
- **Dependencies:** U2, U3.
- **Files:**
  - `src/web/inbound.rs`
  - `src/web/outbound.rs`
  - `src/web/mod.rs`
  - `templates/inbound_form.html`
  - `templates/outbound_list.html`
  - `templates/outbound_form.html`
  - `templates/sidebar_nav.html`
  - `tests/inbound.rs`
- **Approach:**
  1. 新增 `src/web/outbound.rs` 路由模块与对应的模版，在侧边栏导航增加「出站管理」菜单项。
  2. 提供出站的新建与编辑表单，支持常用协议（Socks5/HTTP/SS/VMess/VLESS）的参数输入与校验。
  3. 在 `src/web/inbound.rs` 的编辑与新建表单中，查询已配置的活跃出站列表传递给 `inbound_form.html`。
  4. 在 `templates/inbound_form.html` 中增加下拉选择框 `<select name="outbound_id">`，允许管理员在保存入站时绑定链式代理出站。
  5. 变更后触发 Xray 配置重载应用生效。
- **Test Scenarios:**
  - 完整 HTTP 测试：创建出站 -> 新建入站并选择该出站 -> 校验入站表单保存成功并在后台列表展示链式标记。
  - 切换出站回直连模式，保存后配置正确恢复为无链式路由。

---

## Verification Contract

本地在提交与推送前必须按规范依次执行并全部通过：

1. **代码格式检查：**
   ```bash
   cargo fmt --check
   ```
2. **Clippy 静态检查：**
   ```bash
   cargo clippy --all-targets -- -D warnings
   ```
3. **测试套件验证：**
   ```bash
   cargo test
   ```

---

## Definition of Done

- [ ] 后台侧边栏与系统设置页中清晰可见 3R-UI 面板版本号标识。
- [ ] 数据库迁移 `0016_outbound_proxy.sql` 正确执行，支持增删改查出站代理。
- [ ] 入站可绑定出站代理，Xray `config.json` 正确渲染 `outbounds` 与 `routing.rules`。
- [ ] 集成测试全面覆盖版本展示与链式出站生成逻辑。
- [ ] `cargo fmt`、`cargo clippy`、`cargo test` 全数通过，无 Warning，无回归破坏。
