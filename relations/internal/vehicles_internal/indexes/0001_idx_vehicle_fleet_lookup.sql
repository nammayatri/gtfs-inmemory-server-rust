CREATE INDEX idx_vehicle_fleet_lookup ON public.vehicles_internal USING btree (fleet_no, gtfs_id) WHERE (deleted = false);
