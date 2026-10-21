-- Whether a stop is a depot (docs/gtfs-editor.md section 23, Delhi DTC BMS
-- tender RFP 4.8). A property of the stop itself, independent of whether its
-- geofence boundary has been surveyed yet (0034) -- a stop can be flagged as a
-- depot before anyone has walked its fence.
--
-- Additive: an older image never reads the column. Safe to run twice.

BEGIN;

ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS is_depot boolean NOT NULL DEFAULT false;

COMMIT;
