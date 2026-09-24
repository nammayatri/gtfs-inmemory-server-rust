-- feed_config/update: a feed's data source ('db' or 'preprocessed') is changed
-- through a draft like everything else - added to a change set, submitted,
-- approved by someone else, committed (docs/gtfs-editor.md section 3's "Feed data
-- source"). A change's entity may now be 'feed_config'; its entity_key is the
-- gtfs_id and its after {data_source}.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_entity_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_entity_check
    CHECK (entity IN ('stop', 'route', 'route_stops', 'station', 'feed_config'));

COMMIT;
