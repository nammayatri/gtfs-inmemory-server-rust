CREATE INDEX idx_del_type_time ON public.duty_event_logs USING btree (gtfs_id, event_type, created_at);
