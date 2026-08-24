CREATE INDEX rp_route_order_idx ON public.route_point_internal USING btree (route_id, route_order) WHERE (deleted = false);
