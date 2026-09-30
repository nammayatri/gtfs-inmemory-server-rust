-- Whether a feed is served from its stages.
--
-- A route built from stages used to be kept in two places: its stages, and a
-- copy of what they say in gtfs_route_stop, rewritten on every stage edit so the
-- two never differed. The stage mapper (scripts/map_mtc_stages.py) no longer
-- writes gtfs_route_stop - for chennai_bus that table is MTC's own data, read to
-- learn which stops belong to a stage and never written - so the copy is gone
-- and the stages ARE the route.
--
-- That is a property of the feed, not of a route, and it is opt-in:
--
--   use_stages = true    a route is what its stages say, and nothing else. Its
--                        stops are read from them everywhere (the dashboard,
--                        the export, what passengers are served), and an edit to
--                        a stage reaches its routes by being read, not by a row
--                        being rewritten. gtfs_route_stop is neither read nor
--                        written for the feed: a route with no stages has no
--                        stops until it is given some, whatever rows of it that
--                        table still holds.
--
--   use_stages = false   the feed does not have stages at all. A route is its
--                        rows in gtfs_route_stop, the stop-list editor edits
--                        them, and nothing about stages is offered or applied.
--                        This is every feed until somebody turns it on.
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS use_stages boolean NOT NULL DEFAULT false;

-- A feed that has already been mapped onto stages is one that uses them.
UPDATE gtfs_feed f SET use_stages = true
 WHERE NOT f.use_stages
   AND EXISTS (SELECT 1 FROM gtfs_route_stage rs WHERE rs.gtfs_id = f.gtfs_id);

-- What a route's stops ARE, whichever way the feed keeps them. Every reader
-- that means "the stops of this route" reads one of the two views below, so the
-- rule lives in one place: gtfs_route_stop is only ever written through the
-- stop-list editor and only ever read directly by the code that writes it.
--
-- The columns are gtfs_route_stop's, in its order, so a query written against
-- the table reads either view unchanged. The two hold the same rows and differ
-- only in how a row's `sequence` is worked out, which decides what each is
-- fast at:
--
--   gtfs_route_stop_effective      counts the stops before a row. A filter on
--                                  stop_id or route_id reaches an index, so
--                                  this is the one for "which routes call
--                                  here" and "the stops of this route": 6 ms
--                                  and 1 ms on chennai_bus, where numbering
--                                  the rows costs 127 ms for the first.
--
--   gtfs_route_stop_effective_all  numbers the rows with row_number(). One
--                                  sort for the whole feed: 250 ms to read
--                                  all 89,655 rows where counting costs 1.4 s.
--                                  For the loader and the export, which read
--                                  everything and filter on nothing.
CREATE OR REPLACE VIEW gtfs_route_stop_effective AS
-- a feed that does not use stages: its routes' own rows, exactly as they are.
-- A feed that does contributes nothing here - not the rows of a route that has
-- no stages yet, and not another stop order of one that has.
SELECT rs.gtfs_id, rs.route_id, rs.sequence, rs.stop_id, rs.stop_type, rs.stage_no,
       rs.stage_name, rs.marker_id, rs.marker_lat, rs.marker_lon, rs.provider_id,
       rs.provenance, rs.updated_at, rs.updated_by, rs.marker_name,
       rs.stop_name_override, rs.pattern_key, rs.pickup_type, rs.drop_off_type,
       rs.timepoint, rs.stop_sequence, rs.continuous_pickup, rs.continuous_drop_off,
       rs.shape_dist_traveled, rs.pickup_booking_rule_id, rs.drop_off_booking_rule_id,
       rs.stop_headsign
  FROM gtfs_route_stop rs
 WHERE NOT EXISTS (SELECT 1 FROM gtfs_feed f WHERE f.gtfs_id = rs.gtfs_id AND f.use_stages)
UNION ALL
-- a feed served from stages: what each route's stages say, flattened. The per-row GTFS
-- fields a stage cannot hold are empty, and the provider id is the one the
-- route's own rows usually carry, since it belongs to the route and not to any
-- stop of it.
SELECT rs.gtfs_id, rs.route_id,
       -- the row's place in the route: the stops of the stages before it, plus
       -- its place in its own. Counted rather than numbered with row_number(),
       -- so a filter on stop_id reaches gtfs_stage_stop's index instead of
       -- waiting for the whole feed to be flattened first.
       (SELECT count(*)::integer
          FROM gtfs_route_stage b
          JOIN gtfs_stage_stop bs ON bs.gtfs_id = b.gtfs_id AND bs.stage_id = b.stage_id
                                  AND bs.direction = b.direction
         WHERE b.gtfs_id = rs.gtfs_id AND b.route_id = rs.route_id
           AND NOT b.variant_id IS DISTINCT FROM rs.variant_id
           AND (b."position", bs."position") <= (rs."position", ss."position")) AS sequence,
       ss.stop_id, ss.stop_type, rs.stage_no, st.name AS stage_name,
       ss.marker_id, ss.marker_lat, ss.marker_lon,
       (SELECT mode() WITHIN GROUP (ORDER BY x.provider_id)
          FROM gtfs_route_stop x
         WHERE x.gtfs_id = rs.gtfs_id AND x.route_id = rs.route_id AND x.pattern_key = 1) AS provider_id,
       NULL::jsonb AS provenance, rs.updated_at, rs.updated_by,
       ss.marker_name, ss.stop_name_override, 1::smallint AS pattern_key,
       NULL::smallint AS pickup_type, NULL::smallint AS drop_off_type,
       NULL::smallint AS timepoint, NULL::integer AS stop_sequence,
       NULL::smallint AS continuous_pickup, NULL::smallint AS continuous_drop_off,
       NULL::double precision AS shape_dist_traveled,
       NULL::text AS pickup_booking_rule_id, NULL::text AS drop_off_booking_rule_id,
       NULL::text AS stop_headsign
  FROM gtfs_route_stage rs
  JOIN gtfs_feed f ON f.gtfs_id = rs.gtfs_id AND f.use_stages
  JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
  JOIN gtfs_stage st ON st.gtfs_id = rs.gtfs_id AND st.stage_id = rs.stage_id
                    AND st.direction = rs.direction
  JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id
                         AND ss.direction = rs.direction
 WHERE NOT rs.variant_id IS DISTINCT FROM r.active_variant_id;

COMMENT ON VIEW gtfs_route_stop_effective IS
  'The stops of every route, for a read that names a stop or a route: from its stages on a feed that uses them (and only from them), from gtfs_route_stop on one that does not.';

CREATE OR REPLACE VIEW gtfs_route_stop_effective_all AS
SELECT rs.gtfs_id, rs.route_id, rs.sequence, rs.stop_id, rs.stop_type, rs.stage_no,
       rs.stage_name, rs.marker_id, rs.marker_lat, rs.marker_lon, rs.provider_id,
       rs.provenance, rs.updated_at, rs.updated_by, rs.marker_name,
       rs.stop_name_override, rs.pattern_key, rs.pickup_type, rs.drop_off_type,
       rs.timepoint, rs.stop_sequence, rs.continuous_pickup, rs.continuous_drop_off,
       rs.shape_dist_traveled, rs.pickup_booking_rule_id, rs.drop_off_booking_rule_id,
       rs.stop_headsign
  FROM gtfs_route_stop rs
 WHERE NOT EXISTS (SELECT 1 FROM gtfs_feed f WHERE f.gtfs_id = rs.gtfs_id AND f.use_stages)
UNION ALL
SELECT v.gtfs_id, v.route_id, v.sequence, v.stop_id, v.stop_type, v.stage_no,
       v.stage_name, v.marker_id, v.marker_lat, v.marker_lon,
       (SELECT mode() WITHIN GROUP (ORDER BY x.provider_id)
          FROM gtfs_route_stop x
         WHERE x.gtfs_id = v.gtfs_id AND x.route_id = v.route_id AND x.pattern_key = 1) AS provider_id,
       NULL::jsonb AS provenance, now() AS updated_at, NULL::text AS updated_by,
       v.marker_name, v.stop_name_override, 1::smallint AS pattern_key,
       NULL::smallint AS pickup_type, NULL::smallint AS drop_off_type,
       NULL::smallint AS timepoint, NULL::integer AS stop_sequence,
       NULL::smallint AS continuous_pickup, NULL::smallint AS continuous_drop_off,
       NULL::double precision AS shape_dist_traveled,
       NULL::text AS pickup_booking_rule_id, NULL::text AS drop_off_booking_rule_id,
       NULL::text AS stop_headsign
  FROM gtfs_route_stop_from_stages v
  JOIN gtfs_feed f ON f.gtfs_id = v.gtfs_id AND f.use_stages;

COMMENT ON VIEW gtfs_route_stop_effective_all IS
  'The same rows as gtfs_route_stop_effective, numbered in one pass: for a read of the whole feed.';
