-- A stop's geofence (0034) defaults to a circle, but a surveyor sometimes
-- needs the boundary to follow a depot's actual fence line or a stop's real
-- kerb footprint rather than a disc. `shape` says which of the two the row
-- carries; exactly one of `radius_m` or the `polygon_*` pair is set, enforced
-- below rather than left to the application. `lat`/`lon` stay required either
-- way: the boundary's centre for a circle, a reference point (picked by
-- whoever draws it, typically the stop's own position) for a polygon, used
-- for map centring and nothing else.
--
-- `polygon_lat`/`polygon_lon` follow `gtfs_shape.shape_pt_lat/lon` (0023):
-- parallel arrays, one geofence per row, rather than a point-per-row table.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_geofence
    ALTER COLUMN radius_m DROP NOT NULL;

ALTER TABLE gtfs_geofence
    ADD COLUMN IF NOT EXISTS shape text NOT NULL DEFAULT 'circle',
    ADD COLUMN IF NOT EXISTS polygon_lat double precision[],
    ADD COLUMN IF NOT EXISTS polygon_lon double precision[];

ALTER TABLE gtfs_geofence DROP CONSTRAINT IF EXISTS gtfs_geofence_shape_check;
ALTER TABLE gtfs_geofence ADD CONSTRAINT gtfs_geofence_shape_check CHECK (
    (shape = 'circle'
        AND radius_m IS NOT NULL
        AND polygon_lat IS NULL AND polygon_lon IS NULL)
    OR
    (shape = 'polygon'
        AND radius_m IS NULL
        AND polygon_lat IS NOT NULL AND polygon_lon IS NOT NULL
        AND cardinality(polygon_lat) = cardinality(polygon_lon)
        AND cardinality(polygon_lat) >= 3)
);

ALTER TABLE gtfs_geofence DROP CONSTRAINT IF EXISTS gtfs_geofence_shape_enum_check;
ALTER TABLE gtfs_geofence ADD CONSTRAINT gtfs_geofence_shape_enum_check
    CHECK (shape IN ('circle', 'polygon'));

COMMIT;
