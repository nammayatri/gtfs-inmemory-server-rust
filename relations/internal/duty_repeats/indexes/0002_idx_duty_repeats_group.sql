CREATE INDEX idx_duty_repeats_group ON public.duty_repeats USING btree (trip_group_id) WHERE (NOT deleted);
