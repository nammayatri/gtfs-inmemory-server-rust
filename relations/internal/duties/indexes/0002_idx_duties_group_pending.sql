CREATE INDEX idx_duties_group_pending ON public.duties USING btree (duty_group_id, scheduled_start_at) WHERE (status IN ('upcoming', 'active') AND NOT deleted);
