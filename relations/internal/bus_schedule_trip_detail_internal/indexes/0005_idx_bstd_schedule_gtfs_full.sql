CREATE INDEX idx_bstd_schedule_gtfs_full ON public.bus_schedule_trip_detail_internal USING btree (schedule_trip_id, gtfs_id);
