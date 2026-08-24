CREATE INDEX rp_bus_stop_idx ON public.route_point_internal USING btree (bus_stop_id) WHERE (deleted = false);
