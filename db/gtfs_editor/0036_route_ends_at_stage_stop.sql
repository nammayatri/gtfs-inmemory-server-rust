-- A route ends at the stage stop of its last stage (docs/gtfs-editor.md
-- section 19, 2026-11-04).
--
-- A stage is shared by every route that runs it, and the stops it lists after
-- its stage stop belong to the routes that carry on through it. A route whose
-- LAST stage it is does not go on: of that stage it takes only the rows up to
-- and including the stage's stage stop - its first NEW STOP, or a JUMP STOP
-- opening it - so markers drawing the way into the stop stay. On master v102,
-- 294 of 3,716 routes built from stages called at 1-8 stops past their last
-- stage stop, 625 stop calls in all (route 545 ended at ADYAR DEPOT, then called at THIRUVANMIYUR JAYANTHI
-- THEATRE and L.B ROAD). A route of one stage keeps it whole, and so does a last
-- stage with no stage stop.
--
-- The two views that flatten stages (0025's gtfs_route_stop_from_stages, under
-- 0027's gtfs_route_stop_effective_all, and 0027's gtfs_route_stop_effective)
-- are redefined with the rule; their columns, and their order, are unchanged.
-- Rows the rule drops come after every row a route keeps, so no kept row's
-- sequence moves. The editor's own flattening (validation::last_stage_len,
-- stages::flatten_route) applies the same rule; a build with it needs this.
--
-- How it is worked out matters for speed (measured on master v102's 3,716
-- routes, 41,697 stage links, locally): a test written as EXISTS subqueries over
-- both the link and the stop turned "which routes call at this stop" from a walk
-- in through the stop index (0.4 ms) into a scan of every link (73 ms). So:
--   gtfs_route_stop_effective      reads each link's first and last position and
--                                  its stage's stage stop in LATERAL subqueries
--                                  that depend on the link alone; the planner
--                                  still starts from the stop or the route asked
--                                  for, and the cut is a comparison.
--   gtfs_route_stop_from_stages    takes them from window functions over the
--                                  sort it already makes for row_number().
--
-- Safe to run twice.

BEGIN;

CREATE OR REPLACE VIEW gtfs_route_stop_from_stages AS
SELECT x.gtfs_id, x.route_id, x.sequence, x.stop_id, x.stop_type, x.stage_no,
       x.stage_name, x.stop_name_override, x.marker_id, x.marker_name,
       x.marker_lat, x.marker_lon
  FROM (SELECT rs.gtfs_id,
               rs.route_id,
               row_number() OVER (PARTITION BY rs.gtfs_id, rs.route_id
                                  ORDER BY rs.position, ss.position)::int AS sequence,
               ss.stop_id,
               ss.stop_type,
               rs.stage_no,
               st.name AS stage_name,
               ss.stop_name_override,
               ss.marker_id,
               ss.marker_name,
               ss.marker_lat,
               ss.marker_lon,
               rs.position AS link_at,
               ss.position AS row_at,
               min(rs.position) OVER (PARTITION BY rs.gtfs_id, rs.route_id) AS first_link_at,
               max(rs.position) OVER (PARTITION BY rs.gtfs_id, rs.route_id) AS last_link_at,
               min(ss.position) FILTER (WHERE ss.stop_type IN ('NEW STOP', 'JUMP STOP'))
                   OVER (PARTITION BY rs.gtfs_id, rs.route_id, rs.position) AS stage_stop_at
          FROM gtfs_route_stage rs
          JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
          JOIN gtfs_stage st ON st.gtfs_id = rs.gtfs_id AND st.stage_id = rs.stage_id
                            AND st.direction = rs.direction
          JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id
                                 AND ss.direction = rs.direction
         WHERE rs.variant_id IS NOT DISTINCT FROM r.active_variant_id) x
 -- not a row after the stage stop of the last of two or more stages
 WHERE NOT (x.link_at = x.last_link_at AND x.link_at > x.first_link_at
            AND x.row_at > coalesce(x.stage_stop_at, x.row_at));

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
-- a feed served from stages: what each route's stages say, flattened, ending at
-- the stage stop of its last stage. The per-row GTFS fields a stage cannot hold
-- are empty, and the provider id is the one the route's own rows usually carry,
-- since it belongs to the route and not to any stop of it.
SELECT rs.gtfs_id, rs.route_id,
       -- the row's place in the route: the stops of the stages before it, plus
       -- its place in its own. Counted rather than numbered with row_number(),
       -- so a filter on stop_id reaches gtfs_stage_stop's index instead of
       -- waiting for the whole feed to be flattened first. A row the end rule
       -- drops is after every kept row, so counting it changes no kept sequence.
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
  -- the route's list: where it starts and ends
  CROSS JOIN LATERAL (SELECT min(n."position") AS first_link_at, max(n."position") AS last_link_at
                        FROM gtfs_route_stage n
                       WHERE n.gtfs_id = rs.gtfs_id AND n.route_id = rs.route_id
                         AND NOT n.variant_id IS DISTINCT FROM rs.variant_id) lk
  -- this link's stage: where its stage stop is
  CROSS JOIN LATERAL (SELECT min(h."position") AS stage_stop_at
                        FROM gtfs_stage_stop h
                       WHERE h.gtfs_id = rs.gtfs_id AND h.stage_id = rs.stage_id
                         AND h.direction = rs.direction
                         AND h.stop_type IN ('NEW STOP', 'JUMP STOP')) hd
 WHERE NOT rs.variant_id IS DISTINCT FROM r.active_variant_id
   -- not a row after the stage stop of the last of two or more stages
   AND NOT (rs."position" = lk.last_link_at AND rs."position" > lk.first_link_at
            AND ss."position" > coalesce(hd.stage_stop_at, ss."position"));

COMMENT ON VIEW gtfs_route_stop_effective IS
  'The stops of every route, for a read that names a stop or a route: from its stages on a feed that uses them (and only from them, ending at the stage stop of the last stage), from gtfs_route_stop on one that does not.';

COMMIT;
