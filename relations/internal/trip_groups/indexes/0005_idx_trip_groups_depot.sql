CREATE INDEX idx_trip_groups_depot ON public.trip_groups USING btree (gtfs_id, depot_id) WHERE (NOT deleted);
