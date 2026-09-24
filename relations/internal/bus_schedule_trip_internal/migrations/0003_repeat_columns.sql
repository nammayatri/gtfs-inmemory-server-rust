-- Recurrence lives on the schedule trip, not a separate table -- vehicle/driver/conductor/device
-- stay off it deliberately, since generation derives those from the most recent actual waybill
-- instead of duplicating them. generated_till is the watermark: furthest date confirmed handled
-- under the current config; reset to NULL by set_schedule_trip_repeat_config on every edit.
ALTER TABLE public.bus_schedule_trip_internal
  ADD COLUMN repeat_status text NOT NULL DEFAULT 'inactive',
  ADD COLUMN valid_from date,
  ADD COLUMN valid_until date,
  ADD COLUMN recurrence_days smallint[] NOT NULL DEFAULT '{}',
  ADD COLUMN generated_till date;
