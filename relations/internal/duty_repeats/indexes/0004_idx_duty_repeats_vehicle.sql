CREATE INDEX idx_duty_repeats_vehicle ON public.duty_repeats USING btree (gtfs_id, vehicle_number) WHERE (NOT deleted);
