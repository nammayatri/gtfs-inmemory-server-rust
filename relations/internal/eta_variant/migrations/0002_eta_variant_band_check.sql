-- Bands are stored as 'HH:MM' to match how every other schedule time in this database is
-- written (bus_schedule_trip_detail.start_time / end_time), because that is what a band is
-- compared against when resolving a trip's variant. Storing minutes-since-midnight here
-- would mean converting at that comparison for no gain.
--
-- A variant is either time-banded (am_peak) or condition-based (monsoon), never half of
-- each: both columns are set or neither is. A band whose start is later than its end wraps
-- midnight and is deliberately allowed, which is also how a band ending at midnight is
-- written ('20:00' to '00:00'). Start equal to end is rejected as ambiguous between an
-- empty band and a whole-day one; a variant with no band uses NULL for both.
--
-- The format is constrained here rather than left to callers: this table is written only by
-- us, so it can hold a guarantee the upstream schedule tables cannot.

ALTER TABLE ONLY public.eta_variant
    ADD CONSTRAINT eta_variant_band_check CHECK (
        ((band_start_time IS NULL) = (band_end_time IS NULL))
        AND (band_start_time IS NULL OR band_start_time ~ '^([01][0-9]|2[0-3]):[0-5][0-9]$')
        AND (band_end_time IS NULL OR band_end_time ~ '^([01][0-9]|2[0-3]):[0-5][0-9]$')
        AND (band_start_time IS NULL OR band_start_time <> band_end_time)
    );
