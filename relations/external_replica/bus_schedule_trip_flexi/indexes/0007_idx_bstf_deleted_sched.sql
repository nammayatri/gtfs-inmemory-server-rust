CREATE INDEX idx_bstf_deleted_sched ON public.bus_schedule_trip_flexi USING btree (deleted, schedule_trip_id);
