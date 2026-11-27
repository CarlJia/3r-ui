---
title: "feat: 反向代理与证书管理完全解耦及 NAT VPS 独立证书申请支持"
type: feat
date: 2026-11-27
topic: decouple-proxy-and-cert-management
artifact_contract: ce-unified-plan/v1
product_contract_source: ce-plan-bootstrap
execution: code
---

## Goal Capsule

- **Objective**: 将 3R-UI 面板中的“反向代理 (Pingora)”与“TLS 证书管理 (ACME DNS-01 / 本地证书)”在数据模型、后台任务与 Web 交互上完全解耦；使得无需开启反向代理的场景（例如 NAT VPS 仅运行 Hysteria 2 节点，或外部已挂独立反代）也能独立配置 Cloudflare API Token 并自动申请/管理 TLS 证书，彻底解决客户端因证书缺失/自签 Fallback 导致的连接超时与握手失败问题。
- **Means**: 新增独立证书配置存储机制（迁移分离 `proxy_settings` 中的证书字段至通用证书配置，或将证书申请逻辑脱离反代 `enabled` 守卫），让定时续期循环与手动签发接口不再受限于 `proxy_settings.is_enabled()`；优化前端将“反代证书”拆分为独立的“反向代理设置”与“SSL 证书管理”分区/标签，并确保 Hysteria 2 入站能够直接引用已配置域名的正式 ACME 证书。
- **Product authority**: 严格聚焦于反代与证书配置解耦及证书的独立申请与复用，不更改底层 ACME DNS-01 验证逻辑与 Pingora 反代本身的转发规则。
- **Open blockers**: 无。
- **Product Contract preservation**: 新建技术方案（product_contract_source: ce-plan-bootstrap），基于用户关于“反向代理和证书管理要区分开”的明确需求定义。

---

## Product Contract

### Problem Frame

当前 3R-UI 存在以下强耦合设计缺陷：
1. **申请证书强依赖反代开启**：
   在 `src/proxy/cert.rs` 的定时续期循环 `run_renewal_loop` 中，判断首要条件为 `!settings.is_enabled() { continue; }`。即如果用户未勾选“启用反向代理服务”，后台自动续期任务直接跳过；
2. **NAT VPS 场景受阻**：
   对于 NAT 机器（无公网独立 80/443/8443 端口，或通过高位端口/内网穿透访问），管理员通常不需要（也不能）在 VPS 上开启 Pingora 反代面板，但**需要**运行 Hysteria 2 节点（Hy2 协议基于 UDP 端口转发，必须依赖合法的 TLS 证书）。由于反代未启用无法自动申请证书，导致 Hy2 只能 fallback 到 `_fallback` 自签证书；而客户端默认开启证书校验（insecure=0），最终引发客户端连接超时（`timeout`）与握手错误；
3. **UI 逻辑混淆**：
   在 `/admin/proxy` 页面中，域名、端口、路径与 Cloudflare 证书申请被包裹在同一个 Form 表单与同一个卡片内，造成“必须开启反向代理才能填写域名并申请证书”的错觉与限制。

### Actors

- **A1. NAT VPS / 节点管理员**：无需开启面板反向代理，仅需为 Hysteria 2 等入站申请正规可信的 ACME TLS 证书。
- **A2. 标准 VPS 管理员**：既需要开启 Pingora 面板反代，又需要自动配置证书。
- **A3. 外部代理管理员**：前端已有外部 Nginx/Caddy/Cloudflare Tunnel 接入，面板关闭反代，但 Xray/Hy2 入站仍需 3R-UI 自动申请维护证书。

### Key Technical Decisions (KTD)

- **KTD1. 证书申请与续期生命周期脱离反代开关**：
  将证书签发、ACME DNS-01 请求及自动续期循环的触发条件从 `proxy.is_enabled()` 中解绑。无论 Pingora 反代是否开启，只要配置了域名与有效的 Cloudflare API Token（或选择了自动证书模式），定时任务均照常执行续期，确保本地证书库文件（`data_dir/certs/<domain>/fullchain.pem`）始终保持最新。
- **KTD2. 数据库配置平滑兼容与语义解耦**：
  在现有 `proxy_settings` 表（或扩展独立的 `cert_settings`，由于 `proxy_settings` 已经是单例表，保持单例表字段或无损视图兼容，或者将 `proxy_settings` 字段逻辑分拆）中，明确字段权责：
  - 反代配置：`enabled`, `listen_port`, `path`, `sub_path`
  - 证书配置：`domain`（兼作为默认/主证书域名）, `cert_mode`, `cf_api_token`, `cf_zone_id`, `custom_cert_path`, `custom_key_path`, `cert_expires_at`, `last_renewed_at`, `last_error`
  无论反代 `enabled` 是 0 还是 1，证书配置均可独立更新与触发申请。
- **KTD3. UI 界面模块分立与指引优化**：
  在 `/admin/proxy`（或重命名为“反代与证书” / 拆分清晰板块）：
  - 独立卡片 1：**反向代理 (Pingora)** —— 控制是否开启面板反向代理、监听端口、自定义管理路径等；
  - 独立卡片 2：**SSL 证书管理 (Cloudflare ACME / 本地证书)** —— 独立配置域名、Cloudflare API Token、证书模式、手动申请/续期按钮与实时申请日志，明确标注“无论是否开启反向代理，此处申请的证书均会自动应用于 Hysteria 2 等入站及面板反代”。
- **KTD4. 入站证书解析逻辑无缝对接**：
  保证 `resolve_inbound_cert` 在反代未开启（`proxy_settings.enabled == 0`）时，只要数据库中配置了 `domain` 且证书文件存在，仍能直接匹配并使用正式证书，而不会退化至自签 `_fallback` 证书。

### Requirements

- **R1. 独立证书申请**：在反向代理未启用的状态下，管理员提交域名与 Cloudflare Token 后，能够成功触发 ACME 申请并生成证书至 `certs/<domain>/`。
- **R2. 独立自动续期**：后台 `run_renewal_loop` 循环不再检查 `settings.is_enabled()`，只要配置了合法的 `domain`、`cf_api_token` 且 `cert_mode == "cf_dns"`，即在证书临期前 30 天自动通过 DNS-01 质询完成续期。
- **R3. 反代热重载与守护容错**：当反代未启用时，证书续期成功无需向 `proxy_tx` 发送 `Reload` 命令（或发送后 supervisor 正常无感忽略）；若反代处于启用状态，则继续触发热重载。
- **R4. Hysteria 2 入站直接继承证书**：当反向代理关闭时，Hysteria 2 的 `cert_mode=auto` 配置能正常读取并使用主域名证书（或根据自身 SNI 匹配），避免退回自签证书导致客户端超时报错。
- **R5. UI 分区展示与操作独立**：反代启用开关与证书申请表单操作互相独立，反代关闭时证书配置卡片及申请按钮完全可用且有明确状态提示。

---

## High-Level Technical Design

```
+-----------------------------------------------------------------------------------------+
|                                    Web 界面 (/admin/proxy)                               |
+-----------------------------------------------------------------------------------------+
       |                                                               |
       v                                                               v
+----------------------------+                               +----------------------------+
|   模块 1: 反向代理配置       |                               |    模块 2: SSL 证书管理     |
| - 启用反向代理 (enabled)    |                               | - 申请域名 (domain)         |
| - 监听端口 (8443)          |                               | - 证书模式 (cf_dns/custom) |
| - 面板路径 / 订阅路径       |                               | - CF Token & Zone ID       |
| [保存代理设置]              |                               | [保存证书配置] [立即申请证书]|
+----------------------------+                               +----------------------------+
       |                                                               |
       |                                                               v
       |                                             +------------------------------------+
       |                                             | 异步 ACME DNS-01 任务 / 定时续期     |
       |                                             | (无需 proxy.enabled == true)       |
       |                                             +------------------------------------+
       |                                                               |
       v                                                               v
+----------------------------+                       +------------------------------------+
|   Pingora 反向代理服务      |                       |    证书存储目录 (certs/<domain>/)   |
| (根据 proxy.enabled 启动)  | <--- 加载 TLS 证书 ---|    fullchain.pem & privkey.pem     |
+----------------------------+                       +------------------------------------+
                                                                       |
                                                                       v
                                                     +------------------------------------+
                                                     |    Hysteria 2 入站 (Xray)          |
                                                     | (即使反代关闭，也正常使用正规证书!)   |
                                                     +------------------------------------+
```

---

## Implementation Units

### U1. 解耦证书自动续期循环与后台任务

- **Goal**: 移除 `run_renewal_loop` 中对 `proxy.enabled` 的硬性约束，使未启用反向代理的 VPS 也能够自动续期证书。
- **Requirements**: R2, R3
- **Dependencies**: 无
- **Files**:
  - `src/proxy/cert.rs`
- **Approach**:
  1. 在 `run_renewal_loop` 的判断条件中：
     ```rust
     // 原逻辑:
     // if !settings.is_enabled() || settings.cert_mode != "cf_dns" || ...
     // 修改后:
     if settings.cert_mode != "cf_dns"
         || settings.domain.is_empty()
         || settings.cf_api_token.is_empty()
     {
         continue;
     }
     ```
  2. 续期成功后，仅在 `settings.is_enabled()` 时向 `proxy_tx` 发送 `ProxyCommand::Reload`，避免在反代关闭时触发不必要的重载日志。
- **Test scenarios**:
  - 当 `settings.enabled == 0` 但配置了有效域名和 CF Token 时，`run_renewal_loop` 能够检测到证书缺失或过期并触发申请。
  - 续期完成后，反代处于未启动状态时不报错，证书正确写入 `certs/<domain>/`。

---

### U2. 优化手动申请证书与设置保存接口

- **Goal**: 允许管理员在反代关闭或独立保存证书配置时，直接触发证书申请，且不强制开启反代。
- **Requirements**: R1, R3
- **Dependencies**: U1
- **Files**:
  - `src/web/settings.rs`
- **Approach**:
  1. 检查 `save_proxy_settings`：支持表单同时或分别更新反代字段和证书字段，即使 `enabled` 为 false，也允许更新 `domain`、`cert_mode`、`cf_api_token` 等。
  2. 检查 `request_cert_now`：目前该接口仅校验 `settings.domain.is_empty() || settings.cf_api_token.is_empty()`，本就未直接判断 `settings.enabled`；确保在签发成功后，如果反代开启则发送 `ProxyCommand::Reload`，若反代未开启则仅更新数据库状态即可。
- **Test scenarios**:
  - `POST /admin/proxy` 提交 `enabled=false` 及有效域名与 Cloudflare API Token，配置成功保存，数据库中 `domain` 和 `cf_api_token` 正确更新。
  - `POST /admin/proxy/request-cert` 在 `enabled=0` 时被调用，返回成功并在后台启动 ACME 申请流程。

---

### U3. 确保入站协议（Hysteria 2）在反代关闭时正常加载域名证书

- **Goal**: 保证 Xray 在构建 Hysteria 2 入站配置时，即使反代未启用（`proxy.enabled == 0`），依然能够获取到已配置域名的证书文件。
- **Requirements**: R4
- **Dependencies**: U1, U2
- **Files**:
  - `src/xray/config.rs`
  - `src/proxy/cert.rs`
- **Approach**:
  1. 审查 `src/xray/config.rs` 中的 `proxy_domain` 获取逻辑：
     ```rust
     let proxy_domain = crate::proxy::config::get_proxy_settings(pool)
         .await
         .ok()
         .map(|s| s.domain)
         .filter(|d| !d.is_empty());
     ```
     当前实现是直接读取 `s.domain`，并没有过滤 `s.is_enabled()`，这是正确的！
  2. 审查 `resolve_inbound_cert`：确认优先使用 `proxy_domain` 的证书路径，其次是 `inbound.sni` 对应的证书路径。如果存在对应域名的 `fullchain.pem` 和 `privkey.pem`，直接返回，不再回退到自签 `_fallback` 证书。
  3. 增加单元测试与集成测试，覆盖：反代 `enabled == 0`，但已申请域名证书时，`render_hysteria2_inbound` 能够正确加载该域名证书，而不是 `_fallback` 证书。
- **Test scenarios**:
  - 在 `proxy_settings.enabled = 0` 且 `data_dir/certs/hy2.example.com/` 存在有效证书的情况下，Hysteria 2 入站生成的 Xray 配置包含真实的证书路径，而非 `_fallback` 路径。
  - 订阅链接下发时，`insecure` 保持为 0（因使用的是真实有效证书），客户端连接不会因为证书自签或缺失而超时。

---

### U4. 重构前端 `/admin/proxy` 界面，实现反代与证书管理清晰解耦

- **Goal**: 改造前端模板，将“反向代理 (Pingora)”与“SSL 证书管理”拆分为两个独立的配置区，并在导航栏与文案上给予明确清晰的指引。
- **Requirements**: R5
- **Dependencies**: U2, U3
- **Files**:
  - `templates/proxy.html`
  - `templates/sidebar_nav.html`
- **Approach**:
  1. **UI 模块化拆分**：
     - **卡片 1：反向代理设置 (Pingora)**
       - 开关：是否启用反向代理服务（默认关闭或保持原状）
       - 监听端口、面板访问路径、订阅访问路径
       - 附带说明：“提示：如您使用 NAT VPS、端口受限机器或纯节点服务器，无需开启反向代理，只需在下方配置 SSL 证书即可。”
     - **卡片 2：SSL 证书管理 (ACME DNS-01 / 本地证书)**
       - 申请/绑定域名 (FQDN)
       - 证书模式选择（Cloudflare DNS-01 自动申请 / 本地自定义证书）
       - Cloudflare API Token 与 Zone ID（仅在选择 DNS-01 时展示）
       - 自定义证书路径（仅在选择自定义证书时展示）
       - 当前证书状态展示、最近错误展示
       - 操作按钮：【保存证书配置】、【立即申请 / 强制续期证书】
     - **卡片 3：证书目录与已安装文件**（保持并完善现有的 `scan_certs_dir` 列表与下载功能）
  2. **统一或分立表单处理**：
     - 可以让两个卡片使用同一个保存处理接口，或拆分为独立的保存按钮，确保用户单独修改证书或反代时互不干扰。
  3. **导航优化**：
     - 侧边栏保持 `反代证书` 或文案微调为 `反代与证书`，保持直观。
- **Test scenarios**:
  - 访问 `/admin/proxy`，页面清晰呈现“反向代理”与“SSL 证书管理”两个卡片。
  - 在反代关闭状态下，仅在“SSL 证书管理”中填写域名和 Token 并点击保存/申请，能正常触发申请并在页面日志卡片中看到进度。

---

## Verification Contract

### 质量与静态检查
```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

### 自动化测试验证
```bash
cargo test
```
重点验证：
- `tests/proxy_e2e_test.rs`
- `tests/hy2_e2e_test.rs`
- `src/proxy/cert.rs` 中的单元测试
- `src/xray/config.rs` 中的入站证书解析测试

---

## Definition of Done

1. **核心逻辑解耦**：`run_renewal_loop` 和 `request_cert_now` 完全不受 `proxy.enabled` 影响，未开反代的 NAT VPS 也能自动申请与自动续期 TLS 证书。
2. **入站证书继承正常**：Hysteria 2 入站协议在反代关闭时，可正常复用已申请的正式域名证书，客户端不再因为 `_fallback` 自签证书导致握手失败或连接超时。
3. **前端体验明确**：`/admin/proxy` 页面明确拆解为“反向代理设置”与“SSL 证书管理”，交互和视觉上完全解耦。
4. **所有自动化测试与规范检查通过**：`cargo fmt --check`、`cargo clippy` 和 `cargo test` 全部 0 warning 0 error 通过。
