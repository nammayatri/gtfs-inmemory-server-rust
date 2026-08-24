CREATE INDEX idx_bstd_cover ON public.bus_schedule_trip_detail USING btree (schedule_trip_id, route_number_id, end_time) WHERE ((trip_type)::text <> 'dead-trip'::text);
