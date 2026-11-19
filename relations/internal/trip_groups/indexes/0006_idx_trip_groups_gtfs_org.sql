CREATE INDEX idx_trip_groups_gtfs_org ON public.trip_groups USING btree (gtfs_id, organization_id) WHERE (NOT deleted);
