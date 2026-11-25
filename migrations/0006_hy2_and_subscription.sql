-- 0006_hy2_and_subscription.sql: Support Hysteria 2 inbound, multiple active inbounds, and subscription token.

-- 1. Inbounds table protocol & certificate extensions
ALTER TABLE inbounds ADD COLUMN protocol TEXT NOT NULL DEFAULT 'vless';
ALTER TABLE inbounds ADD COLUMN cert_mode TEXT NOT NULL DEFAULT 'auto';
ALTER TABLE inbounds ADD COLUMN custom_cert_path TEXT NOT NULL DEFAULT '';
ALTER TABLE inbounds ADD COLUMN custom_key_path TEXT NOT NULL DEFAULT '';
ALTER TABLE inbounds ADD COLUMN masquerade_url TEXT NOT NULL DEFAULT 'https://news.ycombinator.com/';

-- 2. Drop the one_active_inbound single-active restriction to allow multiple inbounds coexisting
DROP INDEX IF EXISTS one_active_inbound;

-- Ensure listen_port remains unique among active inbounds
CREATE UNIQUE INDEX IF NOT EXISTS idx_inbounds_port_active
    ON inbounds(listen_port) WHERE status = 'active';

-- 3. Clients table subscription token extension
ALTER TABLE clients ADD COLUMN sub_token TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_clients_sub_token
    ON clients(sub_token);

-- 4. Multi-inbound client association table
CREATE TABLE IF NOT EXISTS client_inbounds (
    client_id   INTEGER NOT NULL REFERENCES clients(id) ON DELETE CASCADE,
    inbound_id  INTEGER NOT NULL REFERENCES inbounds(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (client_id, inbound_id)
);

-- Backfill client_inbounds from existing clients.inbound_id
INSERT OR IGNORE INTO client_inbounds (client_id, inbound_id)
SELECT id, inbound_id FROM clients WHERE inbound_id IS NOT NULL;

-- Backfill missing sub_tokens using a random hex string representation
UPDATE clients
SET sub_token = lower(hex(randomblob(16)))
WHERE sub_token IS NULL OR sub_token = '';
