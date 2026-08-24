CREATE INDEX idx_waybills_conductor_status_updated ON public.waybills_internal USING btree (gtfs_id, conductor_token_no, status, updated_at DESC) WHERE (deleted = false);
