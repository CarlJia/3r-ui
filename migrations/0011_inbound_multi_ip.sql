-- 0011_inbound_multi_ip.sql: Support multi-NIC IP binding, allow identical ports across different IPs.

PRAGMA foreign_keys = OFF;

CREATE TABLE inbounds_new (
    id                 INTEGER PRIMARY KEY,
    listen_ip          TEXT    NOT NULL DEFAULT '0.0.0.0',
    listen_port        INTEGER NOT NULL,
    dest               TEXT    NOT NULL,
    sni                TEXT    NOT NULL,
    server_name        TEXT,
    private_key        TEXT    NOT NULL,
    public_key         TEXT    NOT NULL,
    short_id           TEXT    NOT NULL,
    xhttp_path         TEXT,
    protocol           TEXT    NOT NULL DEFAULT 'vless',
    cert_mode          TEXT    NOT NULL DEFAULT 'auto',
    custom_cert_path   TEXT    NOT NULL DEFAULT '',
    custom_key_path    TEXT    NOT NULL DEFAULT '',
    masquerade_url     TEXT    NOT NULL DEFAULT 'https://news.ycombinator.com/',
    port_range         TEXT    NOT NULL DEFAULT '',
    transport_protocol TEXT    NOT NULL DEFAULT 'TCP',
    skip_cert_verify   INTEGER NOT NULL DEFAULT 0,
    udp                INTEGER NOT NULL DEFAULT 1,
    status             TEXT    NOT NULL DEFAULT 'active',
    created_at         INTEGER,
    updated_at         INTEGER
);

INSERT INTO inbounds_new (
    id, listen_ip, listen_port, dest, sni, server_name, private_key, public_key,
    short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path,
    masquerade_url, port_range, transport_protocol, skip_cert_verify, udp,
    status, created_at, updated_at
)
SELECT
    id, '0.0.0.0', listen_port, dest, sni, server_name, private_key, public_key,
    short_id, xhttp_path, protocol, cert_mode, custom_cert_path, custom_key_path,
    masquerade_url, port_range, transport_protocol, skip_cert_verify, udp,
    status, created_at, updated_at
FROM inbounds;

DROP TABLE inbounds;

ALTER TABLE inbounds_new RENAME TO inbounds;

DROP INDEX IF EXISTS idx_inbounds_port_active;

CREATE UNIQUE INDEX IF NOT EXISTS idx_inbounds_ip_port_active
    ON inbounds(listen_ip, listen_port) WHERE status = 'active';

PRAGMA foreign_keys = ON;
