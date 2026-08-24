CREATE INDEX idx_fleet_obu_id ON public.fleet_obu_mapping_internal USING btree (gtfs_id, obu_id) WHERE (deleted = false);
