CREATE INDEX idx_waybills_vehicle_status_priority ON public.waybills USING btree (vehicle_no, status, updated_at DESC);
