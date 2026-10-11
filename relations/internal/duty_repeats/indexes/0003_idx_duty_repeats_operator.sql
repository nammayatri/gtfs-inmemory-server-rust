CREATE INDEX idx_duty_repeats_operator ON public.duty_repeats USING btree (gtfs_id, operator_id) WHERE (NOT deleted);
