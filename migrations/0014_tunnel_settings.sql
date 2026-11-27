-- 0014_tunnel_settings.sql: Cloudflare Tunnels (cloudflared) 守护配置与状态
--
-- 单例配置行 (id = 1)：
--   - enabled: 是否启用 Cloudflare Tunnel 穿透守护 (0 = 禁用, 1 = 启用)
--   - tunnel_token: Cloudflare 控制台生成的 Tunnel Token (Base64 凭据字符串)
--   - custom_path: 用户自定义的 cloudflared 二进制执行路径 (留空则使用系统探测路径)
--   - last_status: 最近一次运行/退出状态说明
--   - updated_at: 配置更新时间戳

CREATE TABLE IF NOT EXISTS tunnel_settings (
    id           INTEGER PRIMARY KEY CHECK (id = 1),
    enabled      INTEGER NOT NULL DEFAULT 0,
    tunnel_token TEXT    NOT NULL DEFAULT '',
    custom_path  TEXT    NOT NULL DEFAULT '',
    last_status  TEXT    NOT NULL DEFAULT '',
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT OR IGNORE INTO tunnel_settings (
    id, enabled, tunnel_token, custom_path, last_status
) VALUES (
    1, 0, '', '', ''
);
