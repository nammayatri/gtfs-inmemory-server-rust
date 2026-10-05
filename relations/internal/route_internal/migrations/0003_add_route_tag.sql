-- Named route_tag, not route_type: route_type_id here is already the GTFS mode.
-- Nullable free text; an untagged route keeps today's untagged fare, so this is inert until used.

ALTER TABLE public.route_internal ADD COLUMN IF NOT EXISTS route_tag text;
