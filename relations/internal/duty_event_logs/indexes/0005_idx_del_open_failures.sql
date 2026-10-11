CREATE INDEX idx_del_open_failures ON public.duty_event_logs USING btree (gtfs_id, created_at) WHERE (event_type = 'GENERATION_FAILURE' AND resolved_at IS NULL);
