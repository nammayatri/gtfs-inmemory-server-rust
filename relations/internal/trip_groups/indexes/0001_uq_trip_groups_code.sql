CREATE UNIQUE INDEX uq_trip_groups_code ON public.trip_groups USING btree (gtfs_id, code) WHERE (NOT deleted);
