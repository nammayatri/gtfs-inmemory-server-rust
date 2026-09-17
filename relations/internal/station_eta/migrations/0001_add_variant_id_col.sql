-- One row per (feed, variant, stop pair) instead of one per (feed, stop pair), so the same pair can
-- carry a different time at peak, off-peak, or during a disruption.
--
-- Run after eta_variant/migrations/0003: every existing row is assigned its feed's default variant,
-- keeping the current lookup byte-identical.
--
-- gtfs_id is constrained here rather than in 0003 because the backfill joins on it: a NULL would not
-- match, leaving a NULL variant_id and failing the SET NOT NULL below for the wrong reason.

ALTER TABLE public.station_eta
    ALTER COLUMN gtfs_id SET NOT NULL;

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
