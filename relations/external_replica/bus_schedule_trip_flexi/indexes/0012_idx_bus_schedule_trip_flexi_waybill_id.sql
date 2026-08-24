CREATE INDEX idx_bus_schedule_trip_flexi_waybill_id ON public.bus_schedule_trip_flexi USING btree (waybill_id, is_active_trip, trip_number);
