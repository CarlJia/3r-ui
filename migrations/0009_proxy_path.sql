-- 0009_proxy_path.sql: Add path and sub_path columns to proxy_settings table
ALTER TABLE proxy_settings ADD COLUMN path TEXT NOT NULL DEFAULT '';
ALTER TABLE proxy_settings ADD COLUMN sub_path TEXT NOT NULL DEFAULT '/sub';
