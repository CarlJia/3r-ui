-- 0007_mieru_inbound.sql: Support Mieru inbound protocol, port range, transport protocol, and client credentials.

-- 1. Inbounds table Mieru extensions
ALTER TABLE inbounds ADD COLUMN port_range TEXT NOT NULL DEFAULT '';
ALTER TABLE inbounds ADD COLUMN transport_protocol TEXT NOT NULL DEFAULT 'TCP';

-- 2. Clients table Mieru credentials
ALTER TABLE clients ADD COLUMN mieru_username TEXT;
ALTER TABLE clients ADD COLUMN mieru_password TEXT;

-- Backfill missing mieru_username and mieru_password for existing clients
UPDATE clients
SET mieru_username = CASE
    WHEN name IS NOT NULL AND length(trim(name)) > 0 THEN lower(replace(name, ' ', '_'))
    ELSE 'user_' || substr(uuid, 1, 8)
END
WHERE mieru_username IS NULL OR mieru_username = '';

UPDATE clients
SET mieru_password = lower(hex(randomblob(10)))
WHERE mieru_password IS NULL OR mieru_password = '';
