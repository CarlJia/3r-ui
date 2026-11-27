-- 0013_ddns_settings.sql: Cloudflare DDNS 动态域名解析配置与运行状态
--
-- 单例配置行 (id = 1)：
--   - enabled: 是否启用 DDNS 自动同步 (0 = 禁用, 1 = 启用)
--   - cf_api_token: 独立的 Cloudflare API Token (留空则可继承反向代理配置中的 Token)
--   - ipv4_enabled: 是否启用 IPv4 探测与同步 (0 = 禁用, 1 = 启用)
--   - ipv4_domains: IPv4 A 记录目标域名列表 (逗号或换行分隔)
--   - ipv6_enabled: 是否启用 IPv6 探测与同步 (0 = 禁用, 1 = 启用)
--   - ipv6_domains: IPv6 AAAA 记录目标域名列表 (逗号或换行分隔)
--   - check_interval_mins: 检测同步周期（分钟，默认 5）
--   - cf_proxied: Cloudflare CDN 代理状态 (0 = DNS Only，1 = 开启 CDN 代理，默认 0)
--   - auto_sync_public_host: 是否在同步成功后自动更新 system_settings.public_host (0 = 否, 1 = 是，默认 1)
--   - last_ipv4: 上次检测到的公网 IPv4
--   - last_ipv6: 上次检测到的公网 IPv6
--   - last_sync_at: 上次同步执行时间戳
--   - last_status: 上次同步执行结果说明
--   - updated_at: 配置更新时间戳

CREATE TABLE IF NOT EXISTS ddns_settings (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    enabled             INTEGER NOT NULL DEFAULT 0,
    cf_api_token        TEXT    NOT NULL DEFAULT '',
    ipv4_enabled        INTEGER NOT NULL DEFAULT 1,
    ipv4_domains        TEXT    NOT NULL DEFAULT '',
    ipv6_enabled        INTEGER NOT NULL DEFAULT 0,
    ipv6_domains        TEXT    NOT NULL DEFAULT '',
    check_interval_mins INTEGER NOT NULL DEFAULT 5,
    cf_proxied          INTEGER NOT NULL DEFAULT 0,
    auto_sync_public_host INTEGER NOT NULL DEFAULT 1,
    last_ipv4           TEXT    NOT NULL DEFAULT '',
    last_ipv6           TEXT    NOT NULL DEFAULT '',
    last_sync_at        INTEGER NOT NULL DEFAULT 0,
    last_status         TEXT    NOT NULL DEFAULT '',
    updated_at          INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT OR IGNORE INTO ddns_settings (
    id, enabled, cf_api_token, ipv4_enabled, ipv4_domains,
    ipv6_enabled, ipv6_domains, check_interval_mins, cf_proxied,
    auto_sync_public_host, last_ipv4, last_ipv6, last_sync_at, last_status
) VALUES (
    1, 0, '', 1, '',
    0, '', 5, 0,
    1, '', '', 0, ''
);
