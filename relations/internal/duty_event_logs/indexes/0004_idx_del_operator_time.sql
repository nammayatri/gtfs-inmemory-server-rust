CREATE INDEX idx_del_operator_time ON public.duty_event_logs USING btree (gtfs_id, operator_id, created_at);
