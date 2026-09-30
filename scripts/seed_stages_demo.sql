-- A small feed to try the stage editor on (docs/gtfs-editor.md section 18):
-- 200 stops, 50 stages and 10 routes built from them, in feed `stages_demo`.
-- LOCAL DATABASES ONLY. Run again to put the feed back as it was:
--
--   psql postgres://postgres@127.0.0.1:55432/mtc_internal_master -f scripts/seed_stages_demo.sql
--
-- The stops are copies of real chennai_bus stops (same ids, names and
-- positions) along five long routes, so the map looks right; chennai_bus itself
-- is only read. Each route gives a corridor of 40 stops, cut into 10 stages of
-- 4 (a stage stop and three intermediate stops):
--
--   D1..D5   a whole corridor: its 10 stages
--   D6..D10  a short working of the same corridor: stages 3 to 8
--
-- so stages 3-8 of each corridor are shared by two routes, and an edit to one of
-- them shows up on both. The feed is 'preprocessed': GIMS does not serve it, it
-- is only there to edit.

\set ON_ERROR_STOP on
BEGIN;

-- ---------------------------------------------------------------- clear
DELETE FROM gtfs_change_set       WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_position_review  WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_station_proposal WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_release          WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_route_stage      WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_stage_stop       WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_stage_review     WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_stage            WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_route_stop       WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_pattern          WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_route            WHERE gtfs_id = 'stages_demo';
UPDATE gtfs_stop SET parent_station = NULL WHERE gtfs_id = 'stages_demo' AND parent_station IS NOT NULL;
DELETE FROM gtfs_stop             WHERE gtfs_id = 'stages_demo';
-- the editor writes these two as the demo feed is used; without them the feed
-- cannot be deleted and a re-seed fails on gtfs_agency's foreign key
DELETE FROM gtfs_agency           WHERE gtfs_id = 'stages_demo';
DELETE FROM gtfs_feed             WHERE gtfs_id = 'stages_demo';

INSERT INTO gtfs_feed (gtfs_id, display_name, data_source, agency_name)
VALUES ('stages_demo', 'Stages demo (dummy data)', 'preprocessed', 'MTC');

-- ---------------------------------------------------------------- corridors
-- 40 distinct stops from each of five long routes, in route order, never a
-- stop another corridor already took
CREATE TEMP TABLE demo_corridor (corridor int, i int, stop_id text COLLATE "C") ON COMMIT DROP;
DO $$
DECLARE
    src text;
    n int := 0;
    ids text[];
BEGIN
    FOREACH src IN ARRAY ARRAY['75', '3080', '4059', '2570', '4733', '49', '4293', '3472'] LOOP
        EXIT WHEN n = 5;
        SELECT array_agg(stop_id ORDER BY seq) INTO ids FROM (
            SELECT rs.stop_id, min(rs.sequence) AS seq
            FROM gtfs_route_stop rs
            JOIN gtfs_stop s ON s.gtfs_id = rs.gtfs_id AND s.stop_id = rs.stop_id
                            AND NOT s.deleted AND s.location_type = 0
            WHERE rs.gtfs_id = 'chennai_bus' AND rs.route_id = src
              AND rs.stop_type IN ('NEW STOP', 'INTERMEDIATE STOP')
              AND rs.stop_id NOT IN (SELECT stop_id FROM demo_corridor)
            GROUP BY rs.stop_id) x;
        CONTINUE WHEN coalesce(array_length(ids, 1), 0) < 40;
        n := n + 1;
        INSERT INTO demo_corridor SELECT n, i, ids[i] FROM generate_series(1, 40) i;
    END LOOP;
    IF n < 5 THEN
        RAISE EXCEPTION 'found only % corridors of 40 stops in chennai_bus', n;
    END IF;
END $$;

-- ---------------------------------------------------------------- stops
INSERT INTO gtfs_stop (gtfs_id, stop_id, stop_code, name, lat, lon, regional_name, description,
                       position_source, provenance, updated_by)
SELECT 'stages_demo', s.stop_id, s.stop_code, s.name, s.lat, s.lon, s.regional_name, s.description,
       s.position_source, jsonb_build_object('demo_copy_of', 'chennai_bus'), 'seed_stages_demo'
FROM demo_corridor c
JOIN gtfs_stop s ON s.gtfs_id = 'chennai_bus' AND s.stop_id = c.stop_id;

-- ---------------------------------------------------------------- stages
-- stage k of corridor c = stops 4k-3 .. 4k, named after its stage stop
INSERT INTO gtfs_stage (gtfs_id, stage_id, name, description, provenance, updated_by)
SELECT 'stages_demo', format('stg_demo_%s_%s', c.corridor, lpad(((c.i + 3) / 4)::text, 2, '0')),
       upper(s.name), format('Corridor %s, stage %s', c.corridor, (c.i + 3) / 4),
       '{"source": "seed_stages_demo"}'::jsonb, 'seed_stages_demo'
FROM demo_corridor c
JOIN gtfs_stop s ON s.gtfs_id = 'stages_demo' AND s.stop_id = c.stop_id
WHERE c.i % 4 = 1;

INSERT INTO gtfs_stage_stop (gtfs_id, stage_id, position, stop_id, stop_type)
SELECT 'stages_demo', format('stg_demo_%s_%s', corridor, lpad(((i + 3) / 4)::text, 2, '0')),
       (i - 1) % 4 + 1, stop_id,
       CASE WHEN i % 4 = 1 THEN 'NEW STOP' ELSE 'INTERMEDIATE STOP' END
FROM demo_corridor;

-- ---------------------------------------------------------------- routes
-- D<c>: stages 1-10 of corridor c; D<c+5>: stages 3-8 of it
CREATE TEMP TABLE demo_route (route_id text COLLATE "C", corridor int, first_stage int, last_stage int) ON COMMIT DROP;
INSERT INTO demo_route
SELECT 'D' || c, c, 1, 10 FROM generate_series(1, 5) c
UNION ALL
SELECT 'D' || (c + 5), c, 3, 8 FROM generate_series(1, 5) c;

INSERT INTO gtfs_route (gtfs_id, route_id, short_name, long_name, route_type, agency_id, provenance, updated_by)
SELECT 'stages_demo', r.route_id, r.route_id,
       initcap(f.name) || ' To ' || initcap(l.name), 3, 'MTC',
       '{"source": "seed_stages_demo"}'::jsonb, 'seed_stages_demo'
FROM demo_route r
JOIN demo_corridor fc ON fc.corridor = r.corridor AND fc.i = r.first_stage * 4 - 3
JOIN gtfs_stop f ON f.gtfs_id = 'stages_demo' AND f.stop_id = fc.stop_id
JOIN demo_corridor lc ON lc.corridor = r.corridor AND lc.i = r.last_stage * 4
JOIN gtfs_stop l ON l.gtfs_id = 'stages_demo' AND l.stop_id = lc.stop_id;

INSERT INTO gtfs_route_stage (gtfs_id, route_id, position, stage_id, stage_no, updated_by)
SELECT 'stages_demo', r.route_id, k - r.first_stage + 1,
       format('stg_demo_%s_%s', r.corridor, lpad(k::text, 2, '0')), k - r.first_stage + 1, 'seed_stages_demo'
FROM demo_route r, generate_series(r.first_stage, r.last_stage) k;

-- the flattened stop list every reader uses, exactly as the editor derives it
INSERT INTO gtfs_route_stop (gtfs_id, route_id, sequence, stop_id, stop_type, stage_no, stage_name,
                             marker_id, marker_name, marker_lat, marker_lon, stop_name_override,
                             provider_id, updated_by)
SELECT 'stages_demo', rs.route_id,
       row_number() OVER (PARTITION BY rs.route_id ORDER BY rs.position, ss.position),
       ss.stop_id, ss.stop_type, rs.stage_no, st.name,
       ss.marker_id, ss.marker_name, ss.marker_lat, ss.marker_lon, ss.stop_name_override,
       'demo_' || rs.route_id, 'seed_stages_demo'
FROM gtfs_route_stage rs
JOIN gtfs_stage st ON st.gtfs_id = rs.gtfs_id AND st.stage_id = rs.stage_id
JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id
WHERE rs.gtfs_id = 'stages_demo';

-- One stage name its routes do not agree about, so the "Stages to review" queue
-- (section 19.1) has something in it to work through. CHROMEPET MIT GATE is two
-- stages here: corridor 2 runs on to Kadaperi, corridor 3 turns into Chromepet.
UPDATE gtfs_stage SET review = 'head_differs'
 WHERE gtfs_id = 'stages_demo' AND upper(btrim(name)) = 'CHROMEPET MIT GATE';

INSERT INTO gtfs_stage_review (gtfs_id, batch, name, name_key, direction, reason, evidence)
SELECT 'stages_demo', 'seed_stages_demo', 'CHROMEPET MIT GATE', 'CHROMEPET MIT GATE',
       s.direction, 'head_differs',
       jsonb_build_object(
         'lists', count(*),
         'stages', jsonb_agg(s.stage_id ORDER BY s.stage_id),
         'head_names', jsonb_build_array('CHROMEPET MIT GATE', 'CHROMEPET'),
         'routes', (SELECT jsonb_agg(DISTINCT rs.route_id) FROM gtfs_route_stage rs
                     WHERE rs.gtfs_id = 'stages_demo' AND rs.stage_id IN (
                       SELECT stage_id FROM gtfs_stage
                        WHERE gtfs_id = 'stages_demo' AND upper(btrim(name)) = 'CHROMEPET MIT GATE')))
FROM gtfs_stage s
WHERE s.gtfs_id = 'stages_demo' AND upper(btrim(s.name)) = 'CHROMEPET MIT GATE'
GROUP BY s.direction;

SELECT (SELECT count(*) FROM gtfs_stop WHERE gtfs_id = 'stages_demo') AS stops,
       (SELECT count(*) FROM gtfs_stage WHERE gtfs_id = 'stages_demo') AS stages,
       (SELECT count(*) FROM gtfs_route WHERE gtfs_id = 'stages_demo') AS routes,
       (SELECT count(*) FROM gtfs_route_stop WHERE gtfs_id = 'stages_demo') AS route_rows,
       (SELECT count(*) FROM gtfs_stage_review WHERE gtfs_id = 'stages_demo') AS to_review;

COMMIT;
