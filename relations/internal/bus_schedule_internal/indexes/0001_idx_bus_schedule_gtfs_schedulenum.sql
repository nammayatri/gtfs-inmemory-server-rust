CREATE INDEX idx_bus_schedule_gtfs_schedulenum ON public.bus_schedule_internal USING btree (gtfs_id, schedule_number) WHERE (deleted = false);
