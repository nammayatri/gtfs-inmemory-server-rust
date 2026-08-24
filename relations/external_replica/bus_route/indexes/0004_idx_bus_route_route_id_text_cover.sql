CREATE INDEX idx_bus_route_route_id_text_cover ON public.bus_route USING btree (((route_id)::text)) INCLUDE (route_number);
