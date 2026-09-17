-- Every read decodes these as non-nullable and every write supplies them; the original import left
-- them nullable. Aborts loudly on an existing NULL, which is data to delete rather than constrain.
-- gtfs_id is not here: 0001's backfill joins on it and constrains it first.

ALTER TABLE public.station_eta
    ALTER COLUMN source_station_code SET NOT NULL,
    ALTER COLUMN destination_station_code SET NOT NULL,
    ALTER COLUMN eta_in_seconds SET NOT NULL;
