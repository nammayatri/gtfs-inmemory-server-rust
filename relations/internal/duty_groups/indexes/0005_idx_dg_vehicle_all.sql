CREATE INDEX idx_dg_vehicle_all ON public.duty_groups USING btree (gtfs_id, vehicle_number, operation_date) WHERE (NOT deleted);
