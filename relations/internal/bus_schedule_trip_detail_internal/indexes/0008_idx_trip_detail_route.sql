CREATE INDEX idx_trip_detail_route ON public.bus_schedule_trip_detail_internal USING btree (route_number_id);
