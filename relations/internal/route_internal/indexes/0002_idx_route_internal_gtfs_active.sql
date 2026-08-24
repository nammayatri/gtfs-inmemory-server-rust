CREATE INDEX idx_route_internal_gtfs_active ON public.route_internal USING btree (gtfs_id, route_number) WHERE (deleted = false);
