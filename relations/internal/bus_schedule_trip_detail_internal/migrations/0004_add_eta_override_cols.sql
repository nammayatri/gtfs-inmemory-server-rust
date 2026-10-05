-- Held on the trip row, not a side table: every schedule read already joins it, so resolving an
-- override costs no extra relation. Scoped to the schedule trip, which is the permanent template.
--
-- The window is matched against the trip's own clock (duty_date + start_time/end_time in IST), not
-- against now(): one schedule trip is re-run by a new waybill each duty date, so an absolute expiry
-- could not say which run was meant. Evaluated in the read query, so no sweeper is needed.
--
-- Nullable with no default: catalog-only, rewrites nothing.

ALTER TABLE public.bus_schedule_trip_detail_internal
    ADD COLUMN eta_override_variant_id text,
    ADD COLUMN eta_override_effective_from timestamp(6) with time zone,
    ADD COLUMN eta_override_effective_untill timestamp(6) with time zone;
