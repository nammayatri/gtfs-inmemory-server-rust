-- Data, not schema. Every station_eta row must name a variant, so each feed needs a default
-- one to point at before variant_id can be made NOT NULL (station_eta/migrations/0001).
--
-- Seeded for the feeds that already have station_eta rows, plus the two fleets this feature
-- targets, so a feed with no rows today still resolves once rows are added. Re-runnable:
-- the conflict target is the partial unique index, so a second run is a no-op rather than a
-- duplicate default.
--
-- gen_random_uuid() is built in from PostgreSQL 13; on older servers enable pgcrypto first.

INSERT INTO public.eta_variant (variant_id, gtfs_id, code, display_name, is_default)
SELECT gen_random_uuid()::text, feed.gtfs_id, 'default', 'Default', true
FROM (
    SELECT DISTINCT gtfs_id FROM public.station_eta WHERE gtfs_id IS NOT NULL
    UNION
    SELECT unnest(ARRAY['chennai_bus', 'kolkata_bus'])
) AS feed(gtfs_id)
ON CONFLICT (gtfs_id, code) WHERE (NOT deleted) DO NOTHING;
