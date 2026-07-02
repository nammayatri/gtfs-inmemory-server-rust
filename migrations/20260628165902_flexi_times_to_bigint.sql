ALTER TABLE bus_schedule_trip_flexi_internal
  ALTER COLUMN trip_start_time TYPE bigint USING NULLIF(trip_start_time, '')::bigint,
  ALTER COLUMN trip_end_time   TYPE bigint USING NULLIF(trip_end_time,   '')::bigint,
  ALTER COLUMN sync_start_time TYPE bigint USING NULLIF(sync_start_time, '')::bigint,
  ALTER COLUMN sync_end_time   TYPE bigint USING NULLIF(sync_end_time,   '')::bigint;
