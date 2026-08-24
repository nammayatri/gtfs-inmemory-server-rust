CREATE INDEX idx_waybills_driver_status_updated ON public.waybills_internal USING btree (gtfs_id, driver_token_no, status, updated_at DESC) WHERE (deleted = false);
