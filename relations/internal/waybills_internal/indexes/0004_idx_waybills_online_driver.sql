CREATE INDEX idx_waybills_online_driver ON public.waybills_internal USING btree (gtfs_id, driver_token_no, updated_at DESC) WHERE (((status)::text = 'online'::text) AND (deleted = false));
