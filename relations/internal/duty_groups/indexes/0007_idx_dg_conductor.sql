CREATE INDEX idx_dg_conductor ON public.duty_groups USING btree (gtfs_id, conductor_token_number, operation_date) WHERE (NOT deleted);
