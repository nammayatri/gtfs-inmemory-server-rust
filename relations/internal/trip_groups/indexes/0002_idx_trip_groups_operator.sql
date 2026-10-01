CREATE INDEX idx_trip_groups_operator ON public.trip_groups USING btree (gtfs_id, operator_id) WHERE (NOT deleted);
