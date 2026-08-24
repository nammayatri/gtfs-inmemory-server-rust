CREATE INDEX idx_bstf_route_schedule ON public.bus_schedule_trip_flexi_internal USING btree (route_number_id, schedule_trip_id, gtfs_id) WHERE ((trip_type)::text <> 'dead-trip'::text);
