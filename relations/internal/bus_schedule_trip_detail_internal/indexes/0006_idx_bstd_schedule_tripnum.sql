CREATE INDEX idx_bstd_schedule_tripnum ON public.bus_schedule_trip_detail_internal USING btree (schedule_trip_id, trip_number) WHERE ((trip_type)::text <> 'dead-trip'::text);
