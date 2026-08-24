CREATE INDEX idx_trip_detail_active ON public.bus_schedule_trip_detail_internal USING btree (schedule_trip_id, is_active_trip);
