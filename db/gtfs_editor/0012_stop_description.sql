-- A description on stops and stations (docs/gtfs-editor.md section 11): free
-- text a passenger may be shown beside the name - GTFS stop_desc. Nullable; a
-- stop without one is served exactly as before. It is written like everything
-- else about a stop: a `stop` or `station` change in a committed draft.
--
-- Safe to run twice.

ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS description text;
