CREATE INDEX idx_waybills_vehicle_latest_z ON public.waybills USING btree (vehicle_no, created_at DESC) INCLUDE (schedule_no) WHERE (vehicle_no IS NOT NULL);
