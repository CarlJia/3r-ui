-- 0012_system_settings.sql: 全局系统与节点连接设置
--
-- 单例配置行 (id = 1)：
--   - public_host: 节点连接地址与订阅默认 Host (可为 VPS 公网 IP 或独立域名)
--   - updated_at: 配置更新时间戳

CREATE TABLE IF NOT EXISTS system_settings (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    public_host TEXT    NOT NULL DEFAULT '',
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT OR IGNORE INTO system_settings (id, public_host) VALUES (1, '');
