-- One row per tag per feed, deleted rows included: re-using a tag means un-deleting its row.
-- CONCURRENTLY builds without an ACCESS EXCLUSIVE write lock; must NOT run inside a transaction.
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS idx_vehicle_tag_internal_gtfs_id_tag_uniq
    ON public.vehicle_tag_internal (gtfs_id, tag);
