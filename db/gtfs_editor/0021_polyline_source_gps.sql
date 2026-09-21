-- Room for a map line traced from the buses' own GPS (docs/gtfs-editor.md
-- section 17). gtfs_route.polyline_source says where a route's line came from;
-- 0001 allowed 'osrm', 'manual' and 'imported', and another branch adds
-- 'upload' (a line from a file). A line the editor consolidated from GPS runs
-- and snapped with OSRM's map-matching is none of those, so it says 'gps'.
--
-- The CHECK below is the union of every value any branch writes, so this file
-- can run before or after the one that adds 'upload', in either order.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_route DROP CONSTRAINT IF EXISTS gtfs_route_polyline_source_check;

ALTER TABLE gtfs_route
    ADD CONSTRAINT gtfs_route_polyline_source_check
    CHECK (polyline_source IS NULL
           OR polyline_source IN ('osrm', 'gps', 'manual', 'upload', 'imported'));

COMMIT;
