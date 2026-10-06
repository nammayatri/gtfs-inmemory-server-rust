CREATE INDEX idx_dg_vehicle ON public.duty_groups USING btree (gtfs_id, vehicle_number, operation_date) WHERE (is_active AND NOT deleted);
