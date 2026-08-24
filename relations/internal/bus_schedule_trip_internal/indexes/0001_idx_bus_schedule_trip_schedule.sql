CREATE INDEX idx_bus_schedule_trip_schedule ON public.bus_schedule_trip_internal USING btree (schedule_id);
