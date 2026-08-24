CREATE INDEX idx_bus_route_id_text ON public.bus_route USING btree (((route_id)::text));
