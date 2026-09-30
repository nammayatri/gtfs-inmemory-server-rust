-- Data, not schema. Every station_eta row must name a variant, so each feed needs a default
-- one to point at before variant_id can be made NOT NULL (station_eta/migrations/0001).
--
-- Seeded for the feeds that already have station_eta rows, plus the two fleets this feature
-- targets, so a feed with no rows today still resolves once rows are added.
--
-- The NOT EXISTS guard makes this re-runnable and covers both unique indexes. No ON CONFLICT: it
-- needs its partial index to exist already and fails at plan time otherwise, which would make this
-- file depend on indexes/ being applied first -- an ordering relations/README.md does not define.
--
-- gen_random_uuid() is built in from PostgreSQL 13; on older servers enable pgcrypto first.

INSERT INTO public.eta_variant (variant_id, gtfs_id, code, display_name, is_default)
SELECT gen_random_uuid()::text, feed.gtfs_id, 'default', 'Default', true
FROM (
    SELECT DISTINCT gtfs_id FROM public.station_eta WHERE gtfs_id IS NOT NULL
    UNION
    SELECT unnest(ARRAY['chennai_bus', 'kolkata_bus'])
) AS feed(gtfs_id)
WHERE NOT EXISTS (
    SELECT 1 FROM public.eta_variant v
     WHERE v.gtfs_id = feed.gtfs_id
       AND NOT v.deleted
       AND (v.is_default OR v.code = 'default')
);
