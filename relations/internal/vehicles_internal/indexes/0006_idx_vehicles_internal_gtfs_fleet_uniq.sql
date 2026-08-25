CREATE UNIQUE INDEX IF NOT EXISTS idx_vehicles_internal_gtfs_fleet_uniq
    ON public.vehicles_internal (gtfs_id, fleet_no)
    WHERE deleted = false;
