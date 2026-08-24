CREATE INDEX idx_fleet_etm_serial ON public.fleet_etm_mapping_internal USING btree (gtfs_id, etm_serial_no) WHERE (deleted = false);
