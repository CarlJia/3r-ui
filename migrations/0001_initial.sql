-- Initial schema for 3r-ui xray-core management panel.
-- Covers Requirements: R3 (config), R15 (SQLite persistence), R11 (clients),
-- R7/R16/R17 (traffic snapshots), R1 (sessions), and partial unique index
-- enforcing "single active inbound" from Assumptions.

CREATE TABLE IF NOT EXISTS admins (
    id            INTEGER PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS inbounds (
    id          INTEGER PRIMARY KEY,
    listen_port INTEGER NOT NULL UNIQUE,
    dest        TEXT    NOT NULL,
    sni         TEXT    NOT NULL,
    server_name TEXT,
    private_key TEXT    NOT NULL,
    public_key  TEXT    NOT NULL,
    short_id    TEXT    NOT NULL,
    status      TEXT    NOT NULL DEFAULT 'active',
    created_at  INTEGER,
    updated_at  INTEGER
);

-- Single active inbound: any inbound row whose status='active' must be unique.
-- Soft-deleted inbounds (status='deleted') and disabled ones do not conflict.
CREATE UNIQUE INDEX IF NOT EXISTS one_active_inbound
    ON inbounds(status) WHERE status = 'active';

CREATE TABLE IF NOT EXISTS clients (
    id         INTEGER PRIMARY KEY,
    inbound_id INTEGER REFERENCES inbounds(id),
    uuid       TEXT    NOT NULL UNIQUE,
    name       TEXT,
    quota_bytes INTEGER NOT NULL,
    expiry_at  INTEGER,
    status     TEXT    NOT NULL DEFAULT 'active',
    created_at INTEGER
);

CREATE TABLE IF NOT EXISTS traffic_snapshots (
    client_id   INTEGER NOT NULL REFERENCES clients(id),
    uplink      INTEGER NOT NULL DEFAULT 0,
    downlink    INTEGER NOT NULL DEFAULT 0,
    recorded_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS traffic_snapshots_client_idx
    ON traffic_snapshots(client_id, recorded_at DESC);

CREATE TABLE IF NOT EXISTS sessions (
    session_id  TEXT    PRIMARY KEY,
    admin_id    INTEGER NOT NULL REFERENCES admins(id),
    expires_at  INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS sessions_expires_idx
    ON sessions(expires_at);
