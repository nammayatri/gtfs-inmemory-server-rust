CREATE INDEX idx_vehicles_lookup ON public.vehicles_internal USING btree (gtfs_id, fleet_no) WHERE (deleted = false);
