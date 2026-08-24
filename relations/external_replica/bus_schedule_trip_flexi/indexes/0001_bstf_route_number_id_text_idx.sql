CREATE INDEX bstf_route_number_id_text_idx ON public.bus_schedule_trip_flexi USING btree (((route_number_id)::text)) WHERE ((trip_type)::text <> 'dead-trip'::text);
