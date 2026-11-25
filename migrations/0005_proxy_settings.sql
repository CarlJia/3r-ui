-- 0005_proxy_settings.sql: Pingora 反向代理与域名访问配置
--
-- 单例配置行 (id = 1)：
--   - enabled: 是否启用内置 Pingora 反向代理 (0=关闭, 1=开启)
--   - listen_port: HTTPS 监听端口，默认 8443
--   - domain: 绑定的访问域名 (如 panel.example.com)
--   - cert_mode: 证书模式 ('cf_dns' 为 Cloudflare DNS-01 自动签发, 'custom' 为本地证书文件)
--   - cf_api_token: Cloudflare API Token (需具备 Zone.DNS:Edit 权限)
--   - cf_zone_id: Cloudflare Zone ID (可选，留空则自动根据域名解析)
--   - custom_cert_path: 自定义证书文件绝对路径 (PEM 格式)
--   - custom_key_path: 自定义私钥文件绝对路径 (PEM 格式)
--   - cert_expires_at: 当前证书到期时间戳 (unix epoch)
--   - last_renewed_at: 上次证书签发/更新时间戳 (unix epoch)
--   - last_error: 最近一次错误信息 (用于在 UI 展示诊断)
--   - updated_at: 配置更新时间戳

CREATE TABLE IF NOT EXISTS proxy_settings (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    enabled           INTEGER NOT NULL DEFAULT 0,
    listen_port       INTEGER NOT NULL DEFAULT 8443,
    domain            TEXT    NOT NULL DEFAULT '',
    cert_mode         TEXT    NOT NULL DEFAULT 'cf_dns',
    cf_api_token      TEXT    NOT NULL DEFAULT '',
    cf_zone_id        TEXT    NOT NULL DEFAULT '',
    custom_cert_path  TEXT    NOT NULL DEFAULT '',
    custom_key_path   TEXT    NOT NULL DEFAULT '',
    cert_expires_at   INTEGER,
    last_renewed_at   INTEGER,
    last_error        TEXT,
    updated_at        INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT OR IGNORE INTO proxy_settings (id, enabled, listen_port, domain, cert_mode)
VALUES (1, 0, 8443, '', 'cf_dns');
