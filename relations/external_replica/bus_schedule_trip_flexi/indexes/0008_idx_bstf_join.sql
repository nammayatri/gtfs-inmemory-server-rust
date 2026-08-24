CREATE INDEX idx_bstf_join ON public.bus_schedule_trip_flexi USING btree (schedule_trip_id, ((route_number_id)::text));
