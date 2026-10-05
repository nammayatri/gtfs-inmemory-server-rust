-- Speeds up the repeater's schedule_trip_id-keyed lookups (no prior index covered it), and
-- doubles as the ON CONFLICT arbiter for create_repeat_waybill's insert so concurrent callers
-- (or a human) can't create two live waybills for the same schedule trip and date. Partial on
-- deleted = false so a soft-deleted waybill doesn't block a real one from being (re-)created,
-- and on status NOT IN ('closed', 'audited') so a waybill that already completed its duty
-- doesn't block that day's slot from being reused -- a fresh duplicate is always created in a
-- non-terminal status (new/upcoming/online), so this still catches every real mistake; it only
-- stops counting a waybill once its own lifecycle is over. This predicate must stay identical to
-- the ON CONFLICT clause in create_repeat_waybills_bulk (operator.rs) -- Postgres requires an
-- exact match to use this index as the conflict arbiter.
CREATE UNIQUE INDEX uq_waybills_schedule_trip_duty_date_active
  ON public.waybills_internal USING btree (gtfs_id, schedule_trip_id, duty_date)
  WHERE deleted = false AND status NOT IN ('closed', 'audited');
