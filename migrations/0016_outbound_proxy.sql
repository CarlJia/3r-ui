-- 0016_outbound_proxy.sql: Support custom outbound proxies and chain relay

CREATE TABLE IF NOT EXISTS outbounds (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    tag           TEXT    NOT NULL UNIQUE,
    name          TEXT    NOT NULL,
    protocol      TEXT    NOT NULL,
    server_addr   TEXT    NOT NULL,
    server_port   INTEGER NOT NULL,
    settings_json TEXT    NOT NULL DEFAULT '{}',
    status        TEXT    NOT NULL DEFAULT 'active',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

ALTER TABLE inbounds ADD COLUMN outbound_id INTEGER REFERENCES outbounds(id) ON DELETE SET NULL;
