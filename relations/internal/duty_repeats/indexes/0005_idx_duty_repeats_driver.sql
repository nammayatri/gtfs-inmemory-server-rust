CREATE INDEX idx_duty_repeats_driver ON public.duty_repeats USING btree (gtfs_id, driver_token_number) WHERE (NOT deleted);
