-- 0010_security_settings.sql: Fail2ban 与系统安全设置
--
-- 单例配置行 (id = 1)：
--   - fail2ban_enabled: 是否启用 Fail2ban 联动 (0=关闭, 1=开启)
--   - ssh_protection: 是否启用 SSH 保护 Jail (0=关闭, 1=开启)
--   - web_protection: 是否启用 3R-UI Web 保护 Jail (0=关闭, 1=开启)
--   - maxretry: 触发封禁的最大失败重试次数，默认 5
--   - findtime_secs: 失败统计时间窗口（秒），默认 600 (10分钟)
--   - bantime_secs: 封禁持续时间（秒），默认 86400 (1天)
--   - whitelist_ips: 逗号或换行分隔的白名单 IP / CIDR 列表
--   - updated_at: 更新时间戳

CREATE TABLE IF NOT EXISTS security_settings (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    fail2ban_enabled  INTEGER NOT NULL DEFAULT 1,
    ssh_protection    INTEGER NOT NULL DEFAULT 1,
    web_protection    INTEGER NOT NULL DEFAULT 1,
    maxretry          INTEGER NOT NULL DEFAULT 5,
    findtime_secs     INTEGER NOT NULL DEFAULT 600,
    bantime_secs      INTEGER NOT NULL DEFAULT 86400,
    whitelist_ips     TEXT    NOT NULL DEFAULT '127.0.0.1/8, ::1',
    updated_at        INTEGER NOT NULL DEFAULT (unixepoch())
);

INSERT OR IGNORE INTO security_settings (id, fail2ban_enabled, ssh_protection, web_protection, maxretry, findtime_secs, bantime_secs, whitelist_ips)
VALUES (1, 1, 1, 1, 5, 600, 86400, '127.0.0.1/8, ::1');
