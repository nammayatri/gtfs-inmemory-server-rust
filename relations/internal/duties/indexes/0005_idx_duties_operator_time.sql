CREATE INDEX idx_duties_operator_time ON public.duties USING btree (gtfs_id, operator_id, scheduled_start_at) WHERE (NOT deleted);
