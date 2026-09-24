-- Speeds up the repeater's schedule_trip_id-keyed lookups (no prior index covered it), and
-- doubles as the ON CONFLICT arbiter for create_repeat_waybill's insert so concurrent callers
-- can't create two waybills for the same schedule trip and date. Partial on deleted = false so
-- a soft-deleted waybill doesn't block a real one from being (re-)created.
CREATE UNIQUE INDEX uq_waybills_schedule_trip_duty_date_active
  ON public.waybills_internal USING btree (gtfs_id, schedule_trip_id, duty_date)
  WHERE deleted = false;
