-- One row per (feed, variant, stop pair) instead of one per (feed, stop pair), so the same
-- pair can carry a different time at peak, off-peak, or during a disruption.
--
-- Run after eta_variant/migrations/0003_seed_default_variants.sql: every existing row is
-- assigned its feed's default variant, which keeps the current lookup byte-identical. The
-- SET NOT NULL then fails loudly if any feed was missed, rather than leaving rows that no
-- lookup can reach.
--
-- ADD COLUMN is nullable with no default, so it is catalog-only and rewrites nothing.

ALTER TABLE public.station_eta
    ADD COLUMN variant_id text;

UPDATE public.station_eta se
   SET variant_id = v.variant_id
  FROM public.eta_variant v
 WHERE v.gtfs_id = se.gtfs_id
   AND v.is_default
   AND NOT v.deleted
   AND se.variant_id IS NULL;

ALTER TABLE public.station_eta
    ALTER COLUMN variant_id SET NOT NULL;
