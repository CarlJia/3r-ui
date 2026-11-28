-- 0015_nodes_and_groups.sql: Distributed Multi-VPS Node Management & Groups

CREATE TABLE IF NOT EXISTS node_groups (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    name        TEXT    NOT NULL UNIQUE,
    tag         TEXT    NOT NULL UNIQUE,
    description TEXT,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS nodes (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT    NOT NULL,
    secret_key        TEXT    NOT NULL UNIQUE,
    is_local          INTEGER NOT NULL DEFAULT 0,
    status            TEXT    NOT NULL DEFAULT 'offline',
    server_address    TEXT,
    group_id          INTEGER REFERENCES node_groups(id) ON DELETE SET NULL,
    last_heartbeat_at INTEGER,
    cpu_percent       REAL,
    memory_used_bytes INTEGER,
    memory_total_bytes INTEGER,
    disk_used_bytes   INTEGER,
    disk_total_bytes  INTEGER,
    services_status   TEXT,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS node_provisioning_tokens (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    token       TEXT    NOT NULL UNIQUE,
    group_id    INTEGER REFERENCES node_groups(id) ON DELETE CASCADE,
    node_name   TEXT,
    expires_at  INTEGER NOT NULL,
    used_at     INTEGER
);

CREATE TABLE IF NOT EXISTS client_node_groups (
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    group_id    INTEGER NOT NULL REFERENCES node_groups(id) ON DELETE CASCADE,
    PRIMARY KEY (client_id, group_id)
);

-- Seed Node 1 as Master Local Node
INSERT OR IGNORE INTO nodes (id, name, secret_key, is_local, status, server_address, created_at, updated_at)
VALUES (1, 'Master Local Node', 'local_master_secret_builtin', 1, 'online', '127.0.0.1', strftime('%s','now'), strftime('%s','now'));
