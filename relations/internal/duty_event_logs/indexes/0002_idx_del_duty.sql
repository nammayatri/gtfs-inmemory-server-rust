CREATE INDEX idx_del_duty ON public.duty_event_logs USING btree (duty_id, created_at) WHERE (duty_id IS NOT NULL);
