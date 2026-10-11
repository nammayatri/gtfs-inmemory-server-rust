CREATE UNIQUE INDEX uq_trips_group_trip_number ON public.trips USING btree (trip_group_id, trip_number) WHERE (NOT deleted);
