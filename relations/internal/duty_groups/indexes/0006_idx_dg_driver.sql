CREATE INDEX idx_dg_driver ON public.duty_groups USING btree (gtfs_id, driver_token_number, operation_date) WHERE (NOT deleted);
