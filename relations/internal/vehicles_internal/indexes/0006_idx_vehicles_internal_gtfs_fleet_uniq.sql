-- CONCURRENTLY builds without an ACCESS EXCLUSIVE write lock; must NOT run inside a transaction.
-- CONCURRENTLY leaves the index INVALID (rather than erroring) on duplicates, so after applying:
--   SELECT indisvalid FROM pg_index WHERE indexrelid = 'public.idx_vehicles_internal_gtfs_fleet_uniq'::regclass;
-- If false, dedupe (gtfs_id, fleet_no) WHERE deleted = false, DROP INDEX, and re-run this file.
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_vehicles_internal_gtfs_fleet_uniq
    ON public.vehicles_internal (gtfs_id, fleet_no)
    WHERE deleted = false;
