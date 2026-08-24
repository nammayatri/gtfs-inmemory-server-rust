CREATE INDEX idx_vehicles_internal_fleet_gtfs_active ON public.vehicles_internal USING btree (fleet_no, gtfs_id) WHERE (deleted = false);
