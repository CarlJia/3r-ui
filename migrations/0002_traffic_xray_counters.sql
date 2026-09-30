-- U7: per-client traffic tracking.
--
-- Add xray_uplink / xray_downlink so each row records the raw xray counter
-- values seen at the moment of the poll. The next poll's delta is computed
-- against these, so a panel restart can rebuild the in-memory snapshot from
-- the latest DB row instead of inventing a phantom delta from 0.

ALTER TABLE traffic_snapshots ADD COLUMN xray_uplink INTEGER NOT NULL DEFAULT 0;
ALTER TABLE traffic_snapshots ADD COLUMN xray_downlink INTEGER NOT NULL DEFAULT 0;