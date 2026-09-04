CREATE INDEX idx_waybills_vehicle_gtfs_updated_full ON public.waybills_internal USING btree (vehicle_no, gtfs_id, updated_at DESC);
