CREATE INDEX stop_latlon_idx ON public.stop_internal USING btree (latitude_current, longitude_current) WHERE (deleted = false);
