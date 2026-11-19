CREATE INDEX idx_dg_depot_date ON public.duty_groups USING btree (gtfs_id, depot_id, operation_date) WHERE (NOT deleted);
