-- Room for the map lines an operator supplies (docs/gtfs-editor.md section 14).
-- gtfs_route.polyline_source has said where a line came from since
-- 0001_create_gtfs_editor.sql, but its CHECK allowed only the three values the
-- nightly build and the road router write: 'osrm', 'manual', 'imported'. The
-- editor now also takes a whole file of lines, and a line from a file is
-- neither drawn by hand nor routed, so it says 'upload'.
--
-- 'imported' stays: the ten lines chennai_bus carries today were written by
-- nandi's loader and say so, and nothing here rewrites them.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_route DROP CONSTRAINT IF EXISTS gtfs_route_polyline_source_check;

ALTER TABLE gtfs_route
    ADD CONSTRAINT gtfs_route_polyline_source_check
    CHECK (polyline_source IS NULL
           OR polyline_source IN ('osrm', 'manual', 'upload', 'imported'));

COMMIT;
