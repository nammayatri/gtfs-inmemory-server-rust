CREATE INDEX idx_trips_gtfs_org ON public.trips USING btree (gtfs_id, organization_id) WHERE (NOT deleted);
