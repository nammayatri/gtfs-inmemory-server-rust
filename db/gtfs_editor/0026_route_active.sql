-- Inactive routes (docs/gtfs-editor.md section 18.17). A route marked inactive
-- keeps everything it has - its stops, stop orders, trips and map line - and
-- GIMS still answers a lookup of it by its id, but leaves it out of every list:
-- the feed's routes, route search, the routes at a stop or between two stops,
-- the stops list (for a stop no active route serves) and the bulk dumps. The
-- zip the feed publishes leaves it and its trips out; the full download keeps
-- it, marked route_active = 0.
--
-- Additive: an older image never reads the column, and GIMS reads it so that
-- every route is active while it is missing. The editor's route screens need
-- it: apply it before the image that has them. Safe to run twice.

BEGIN;

ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS active boolean NOT NULL DEFAULT true;

-- the dashboard lists a feed's inactive routes
CREATE INDEX IF NOT EXISTS gtfs_route_inactive_idx
    ON gtfs_route (gtfs_id) WHERE NOT active AND NOT deleted;

COMMIT;
