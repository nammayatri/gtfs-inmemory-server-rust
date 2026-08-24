CREATE INDEX idx_bstf_cover ON public.bus_schedule_trip_flexi USING btree (schedule_trip_id, route_number_id, end_time) WHERE ((trip_type)::text <> 'dead-trip'::text);
