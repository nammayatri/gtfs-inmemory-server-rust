CREATE INDEX idx_bstd_live_trip ON public.bus_schedule_trip_detail USING btree (schedule_trip_id, route_number_id) INCLUDE (trip_number) WHERE ((trip_type)::text <> 'dead-trip'::text);
