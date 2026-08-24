CREATE INDEX idx_bstd_int_stid_text ON public.bus_schedule_trip_detail_internal USING btree (schedule_trip_id, gtfs_id) WHERE (deleted = false);
