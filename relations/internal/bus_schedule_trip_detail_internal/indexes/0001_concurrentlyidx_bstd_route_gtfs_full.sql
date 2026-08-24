CREATE INDEX concurrentlyidx_bstd_route_gtfs_full ON public.bus_schedule_trip_detail_internal USING btree (route_number_id, gtfs_id);
