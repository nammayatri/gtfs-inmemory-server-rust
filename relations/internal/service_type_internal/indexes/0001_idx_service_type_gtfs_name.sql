CREATE INDEX idx_service_type_gtfs_name ON public.service_type_internal USING btree (gtfs_id, service_type_name) WHERE (deleted = false);
