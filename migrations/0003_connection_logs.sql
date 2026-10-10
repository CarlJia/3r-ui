-- U11: 连接日志持久化。
--
-- 每条 xray 访问日志（从 xray stderr 的 access log 行解析而来）落一条记录。
-- uuid 对应 VLESS client 的 email（= 客户端 UUID）；src_addr 是连接来源
-- ip:port；dest 是目标 host:port；status 是 accepted / rejected；reason 在
-- 拒绝时非空。解析失败的原始行保存在 raw_message，方便后续人工排查。
-- received_at 是面板写入时刻，logged_at 是日志行内的时间戳。

CREATE TABLE connection_logs (
    id INTEGER PRIMARY KEY,
    logged_at INTEGER NOT NULL,
    uuid TEXT NOT NULL,
    src_addr TEXT,
    dest TEXT,
    status TEXT NOT NULL,
    reason TEXT,
    raw_message TEXT NOT NULL,
    received_at INTEGER NOT NULL
);

-- 三个索引覆盖典型筛选路径：时间倒序翻页、按用户筛选、按结果筛选。
CREATE INDEX connection_logs_logged_at_idx ON connection_logs(logged_at);
CREATE INDEX connection_logs_uuid_at_idx ON connection_logs(uuid, logged_at);
CREATE INDEX connection_logs_status_at_idx ON connection_logs(status, logged_at);
