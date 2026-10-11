-- The variant a trip uses when no override is in force. Schedule-level on purpose: these rows
-- are created once with the schedule, so a default set here is inherited by every waybill
-- assigned to that schedule trip and never needs reapplying as waybills turn over daily.
--
-- Nullable: NULL resolves to the feed's default variant, which is what every trip does today.
-- Catalog-only, rewrites nothing.

ALTER TABLE public.bus_schedule_trip_detail_internal
    ADD COLUMN default_variant_id text;
