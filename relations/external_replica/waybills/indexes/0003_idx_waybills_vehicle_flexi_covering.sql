CREATE INDEX idx_waybills_vehicle_flexi_covering ON public.waybills USING btree (vehicle_no, status) INCLUDE (waybill_id, is_flexi, schedule_trip_id, service_type, schedule_no, updated_at);
