CREATE INDEX idx_dg_date ON public.duty_groups USING btree (gtfs_id, operation_date);
