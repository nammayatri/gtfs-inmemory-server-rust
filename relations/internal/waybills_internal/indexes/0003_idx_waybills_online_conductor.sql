CREATE INDEX idx_waybills_online_conductor ON public.waybills_internal USING btree (gtfs_id, conductor_token_no, updated_at DESC) WHERE (((status)::text = 'online'::text) AND (deleted = false));
