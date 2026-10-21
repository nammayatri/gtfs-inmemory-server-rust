-- Stop geofences (docs/gtfs-editor.md section 23, Delhi DTC BMS tender RFP
-- 4.8): a surveyed circular boundary around a stop, read by gps-processor to
-- detect a vehicle's arrival, dwell and departure there. A "terminal" is not
-- stored anywhere -- it is derived at evaluation time from whether the stop is
-- first or last in a route's stop sequence, data this feed already has.
--
-- One row per stop, keyed by stop_id like `gtfs_service` is by service_id: a
-- stop has at most one surveyed boundary at a time. The row simply does not
-- exist until the field survey happens, rather than every stop carrying
-- mostly-empty geofence columns.
--
-- Goes through the same draft -> approve -> commit workflow as every other
-- editor entity (gtfs_change_entity_check, row_version via gtfs_touch_row).
-- Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_geofence (
    gtfs_id     text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    stop_id     text COLLATE "C" NOT NULL,
    lat         double precision NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lon         double precision NOT NULL CHECK (lon BETWEEN -180 AND 180),
    radius_m    integer     NOT NULL CHECK (radius_m > 0),
    row_version integer     NOT NULL DEFAULT 1,
    created_at  timestamptz NOT NULL DEFAULT now(),
    created_by  text,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text,
    PRIMARY KEY (gtfs_id, stop_id),
    FOREIGN KEY (gtfs_id, stop_id) REFERENCES gtfs_stop (gtfs_id, stop_id)
);

DROP TRIGGER IF EXISTS gtfs_geofence_touch ON gtfs_geofence;
CREATE TRIGGER gtfs_geofence_touch BEFORE UPDATE ON gtfs_geofence
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_entity_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_entity_check
    CHECK (entity IN (
        'stop', 'route', 'route_stops', 'station', 'feed_config', 'pattern',
        'timing_profile', 'route_trips', 'service', 'agency',
        'fare_attribute', 'fare_rule', 'timeframe', 'rider_category',
        'fare_media', 'fare_product', 'fare_leg_rule', 'fare_leg_join_rule',
        'fare_transfer_rule', 'area', 'stop_area', 'network', 'route_network',
        'shape', 'transfer', 'pathway', 'level', 'location_group',
        'location_group_stop', 'location', 'booking_rule', 'translation',
        'feed_info', 'attribution',
        'stage', 'route_stages', 'route_variant',
        'geofence'));

COMMIT;
