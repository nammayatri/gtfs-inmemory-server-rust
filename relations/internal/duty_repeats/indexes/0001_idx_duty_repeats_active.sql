CREATE INDEX idx_duty_repeats_active ON public.duty_repeats USING btree (gtfs_id) WHERE (repeat_status = 'active' AND NOT deleted);
