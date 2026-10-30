-- Depends on migrations/0005. Serves vehicle_tags @> ARRAY[code]: /vehicles/query?vehicle_tag= and the in-use check
-- before a vehicle_tag_internal row is deleted.
-- CONCURRENTLY builds without an ACCESS EXCLUSIVE write lock; must NOT run inside a transaction.
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_vehicles_internal_vehicle_tags
    ON public.vehicles_internal USING gin (vehicle_tags)
    WHERE deleted = false;
