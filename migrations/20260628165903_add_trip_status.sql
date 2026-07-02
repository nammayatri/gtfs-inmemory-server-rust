ALTER TABLE bus_schedule_trip_detail_internal
  ADD COLUMN IF NOT EXISTS status text DEFAULT 'active';

ALTER TABLE bus_schedule_trip_detail_internal
  ALTER COLUMN status SET DEFAULT 'active';

UPDATE bus_schedule_trip_detail_internal
  SET status = 'active'
  WHERE status IS NULL;
