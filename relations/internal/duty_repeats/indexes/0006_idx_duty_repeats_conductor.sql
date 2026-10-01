CREATE INDEX idx_duty_repeats_conductor ON public.duty_repeats USING btree (gtfs_id, conductor_token_number) WHERE (NOT deleted);
