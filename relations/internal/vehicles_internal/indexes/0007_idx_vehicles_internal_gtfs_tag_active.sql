-- Depends on migrations/0003 (adds tag_number column) — apply migrations before indexes.
-- CONCURRENTLY builds without an ACCESS EXCLUSIVE write lock; must NOT run inside a transaction.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_vehicles_internal_gtfs_tag_active
    ON public.vehicles_internal (gtfs_id, tag_number)
    WHERE deleted = false;
