CREATE UNIQUE INDEX station_eta_unique_idx ON public.station_eta USING btree (gtfs_id, source_station_code, destination_station_code);
