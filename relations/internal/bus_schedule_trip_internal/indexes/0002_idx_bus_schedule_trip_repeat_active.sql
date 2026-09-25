-- Matches the reconciler's trips query (walk_schedule_trip_repeats); no prior index covered
-- gtfs_id or repeat_status, so every tick was a full table scan.
CREATE INDEX idx_bus_schedule_trip_repeat_active
  ON public.bus_schedule_trip_internal USING btree (gtfs_id, repeat_status)
  WHERE valid_from IS NOT NULL;
