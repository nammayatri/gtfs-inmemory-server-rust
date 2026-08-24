CREATE INDEX idx_bstd_main ON public.bus_schedule_trip_detail USING btree (schedule_trip_id, trip_number);
