-- Superseded by station_eta_variant_unique_idx, which adds variant_id to the same key.
-- Run only after that index exists: dropping first would leave the table with nothing
-- enforcing pair uniqueness, on a table ops write to.
--
-- indexes/0002_station_eta_unique_idx.sql now describes an index that is gone, so the next
-- sync reports it as no longer in the database. Expected, and left in place per the
-- append-only rule.

DROP INDEX IF EXISTS public.station_eta_unique_idx;
