CREATE INDEX idx_route_gtfs_routenumber ON public.route_internal USING btree (gtfs_id, route_number) WHERE (deleted = false);
