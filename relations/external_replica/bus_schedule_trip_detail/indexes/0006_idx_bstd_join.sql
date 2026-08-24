CREATE INDEX idx_bstd_join ON public.bus_schedule_trip_detail USING btree (schedule_trip_id, ((route_number_id)::text));
