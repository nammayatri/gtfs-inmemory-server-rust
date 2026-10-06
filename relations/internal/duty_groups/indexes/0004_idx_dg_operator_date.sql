CREATE INDEX idx_dg_operator_date ON public.duty_groups USING btree (gtfs_id, operator_id, operation_date) WHERE (NOT deleted);
