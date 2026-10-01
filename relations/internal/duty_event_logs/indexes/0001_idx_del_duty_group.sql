CREATE INDEX idx_del_duty_group ON public.duty_event_logs USING btree (duty_group_id, created_at);
