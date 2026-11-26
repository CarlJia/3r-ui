-- 0008_hy2_skip_cert_and_udp.sql: Support skip_cert_verify and udp in inbounds table.

ALTER TABLE inbounds ADD COLUMN skip_cert_verify INTEGER NOT NULL DEFAULT 0;
ALTER TABLE inbounds ADD COLUMN udp INTEGER NOT NULL DEFAULT 1;
