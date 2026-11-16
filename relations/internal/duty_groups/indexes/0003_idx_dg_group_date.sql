CREATE INDEX idx_dg_group_date ON public.duty_groups USING btree (trip_group_id, operation_date);
