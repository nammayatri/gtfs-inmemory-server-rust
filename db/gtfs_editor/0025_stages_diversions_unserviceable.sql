-- Stages, temporary routes and stops out of use
-- (docs/gtfs-editor.md sections 19, 20 and 21).
--
-- Three things that all hang off one idea: a route's stop list is *derived*, so
-- what is edited is smaller than what is read.
--
-- 1. STAGES (section 19). A route is an ordered list of fare stages, and a
--    stage is an ordered list of stops. Most stages are shared: the ~58,800
--    stage runs of chennai_bus's routes are ~10,900 distinct stop sequences,
--    and half the runs belong to a stage six or more routes use. So a stage is
--    stored once and routes point at it:
--
--      gtfs_route --< gtfs_route_stage >-- gtfs_stage --< gtfs_stage_stop >-- gtfs_stop
--
--    Changing one stage changes every route that uses it, in one draft.
--
-- 2. TEMPORARY ROUTES (section 20). Roadworks close a street and a route runs
--    somewhere else for a while: the same route with a different list of
--    stages, worn one at a time.
--
--      gtfs_route_stage.variant_id  -> which list a link belongs to; NULL is normal
--      gtfs_route.active_variant_id -> which list the route wears now; NULL is normal
--
--    There is no table of temporary routes: one *is* its links. It exists while
--    some gtfs_route_stage row carries its id, it is called by that id
--    (mandaveli_1), and it is deleted by deleting those rows.
--
-- 3. STOPS OUT OF USE (section 21). A barricade closes a stop for a few weeks.
--    The stop must stay in the feed, and no trip may call there:
--
--      gtfs_stop.unserviceable -> the stops.txt row stays; no trip calls there
--
--    One flag, read where a trip's times are emitted (the exporter and the GIMS
--    loader), never by taking the stop out of a pattern - a timing profile's
--    offsets are positional over the pattern, so removing a stop would shift
--    every later stop's time. Left in and skipped at emission, every other stop
--    keeps its time to the second and clearing the flag costs nothing.
--
-- gtfs_route_stop stays, unchanged, as the flattened stop list every reader
-- already uses (the GIMS loader, nandi's build, merges, reviews, the public
-- APIs). The editor rewrites a route's rows in the same transaction as any
-- change to its stages:
--
--   gtfs_route_stop(route) = for each gtfs_route_stage of the route
--                            WHERE variant_id IS NOT DISTINCT FROM active_variant_id,
--                            by position: the stage's stops, with the link's
--                            stage_no and the stage's name
--
-- A route with no gtfs_route_stage rows is not built from stages yet: its
-- gtfs_route_stop rows are edited directly, as before, until it is.
--
-- Safe to run twice.

BEGIN;

-- ---------------------------------------------------------------- stages
CREATE TABLE IF NOT EXISTS gtfs_stage (
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    stage_id           text COLLATE "C" NOT NULL,
    -- the stage name every row of the stage carries on a route (stage_name)
    name               text        NOT NULL CHECK (btrim(name) <> ''),
    -- which way along the corridor this stage runs. Two stages share a name
    -- and hold different stops because they are the two directions of it, so
    -- this is what tells them apart in a search, and NULL is a stage that runs
    -- the same either way.
    direction          text        CHECK (direction IS NULL OR direction IN ('up', 'down')),
    -- a note for the people editing, to tell same-named stages apart further:
    -- "towards Guindy", "via the flyover"
    description        text,
    -- where this stage came from: the editor, or the route whose stops became
    -- stages when it was first diverted
    provenance         jsonb,
    -- a stage a route still points at cannot be removed, so a delete is a soft
    -- one, as it is for a stop
    deleted            boolean     NOT NULL DEFAULT false,
    -- what a draft's change to this stage is checked against at commit, so two
    -- people cannot edit one stage past each other
    row_version        integer     NOT NULL DEFAULT 1,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    PRIMARY KEY (gtfs_id, stage_id)
);
-- for a database built by an earlier run of this file, when the table already
-- exists and CREATE TABLE IF NOT EXISTS is a no-op
ALTER TABLE gtfs_stage ADD COLUMN IF NOT EXISTS direction text;
ALTER TABLE gtfs_stage DROP CONSTRAINT IF EXISTS gtfs_stage_direction_check;
ALTER TABLE gtfs_stage ADD CONSTRAINT gtfs_stage_direction_check
    CHECK (direction IS NULL OR direction IN ('up', 'down'));

CREATE INDEX IF NOT EXISTS gtfs_stage_name_trgm ON gtfs_stage USING gin (name gin_trgm_ops);

-- ---------------------------------------------------------------- stage stops
-- One row per position in a stage, holding only what is the stage's to say:
-- which stop, what kind of row it is, and the name this stage shows it under.
-- Everything else about a stop - its name, position, code, platform, whether it
-- is out of use - lives in gtfs_stop and is read by joining, never copied here.
--
-- A ROUTE CORRECTION row is the exception: it is not a stop at all but a point
-- that bends the drawn map line between two stops, so it has no stop_id to join
-- to and carries its own id, name and position. The CHECKs below keep the two
-- kinds apart: a row is either a stop or a marker, never both and never neither.
CREATE TABLE IF NOT EXISTS gtfs_stage_stop (
    gtfs_id            text COLLATE "C" NOT NULL,
    stage_id           text COLLATE "C" NOT NULL,
    position           integer     NOT NULL CHECK (position > 0),
    stop_id            text COLLATE "C",
    stop_type          text        NOT NULL CHECK (stop_type IN
                       ('NEW STOP', 'INTERMEDIATE STOP', 'JUMP STOP', 'ROUTE CORRECTION', 'HIDDEN STOP')),
    -- a map-shaping point (ROUTE CORRECTION), which has no stop to join to
    marker_id          text COLLATE "C",
    marker_name        text,
    marker_lat         double precision,
    marker_lon         double precision,
    -- what this stage calls the stop, where it differs from the stop's own name
    stop_name_override text,
    PRIMARY KEY (gtfs_id, stage_id, position),
    FOREIGN KEY (gtfs_id, stage_id) REFERENCES gtfs_stage (gtfs_id, stage_id) ON DELETE CASCADE,
    FOREIGN KEY (gtfs_id, stop_id)  REFERENCES gtfs_stop (gtfs_id, stop_id),
    CHECK ((stop_type = 'ROUTE CORRECTION') = (stop_id IS NULL)),
    CHECK (stop_type <> 'ROUTE CORRECTION' OR (marker_lat IS NOT NULL AND marker_lon IS NOT NULL)),
    CHECK (stop_name_override IS NULL OR stop_id IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS gtfs_stage_stop_stop_idx ON gtfs_stage_stop (gtfs_id, stop_id) WHERE stop_id IS NOT NULL;

-- ---------------------------------------------------------------- route stages
-- A route's stages in order, per list. stage_no is the route's fare stage
-- number for the stage: almost always the position, but a fare chart may skip
-- or repeat a number, so it is the route's to say. variant_id names the list:
-- NULL is the route's normal one, which is why no route needed backfilling.
CREATE TABLE IF NOT EXISTS gtfs_route_stage (
    gtfs_id            text COLLATE "C" NOT NULL,
    route_id           text COLLATE "C" NOT NULL,
    variant_id         text COLLATE "C"
                       CHECK (variant_id IS NULL OR variant_id ~ '^[A-Za-z0-9_.-]{1,64}$'),
    position           integer     NOT NULL CHECK (position > 0),
    stage_id           text COLLATE "C" NOT NULL,
    stage_no           integer     NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    FOREIGN KEY (gtfs_id, route_id) REFERENCES gtfs_route (gtfs_id, route_id) ON DELETE CASCADE,
    FOREIGN KEY (gtfs_id, stage_id) REFERENCES gtfs_stage (gtfs_id, stage_id)
);
-- the key over (list, position). It is a unique index and not a primary key
-- because a primary key cannot hold NULL, and the normal list's variant_id
-- genuinely is NULL.
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_route_stage_key
    ON gtfs_route_stage (gtfs_id, route_id, COALESCE(variant_id, ''), position);
CREATE INDEX IF NOT EXISTS gtfs_route_stage_stage_idx ON gtfs_route_stage (gtfs_id, stage_id);

-- ---------------------------------------------------------------- which list is live
-- No foreign key: the lists are the links, so what this names is checked where
-- it is set (variants.rs), not by a table of its own.
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS active_variant_id text COLLATE "C";

-- the Diversions page, and "is this route diverted" on a route read
CREATE INDEX IF NOT EXISTS gtfs_route_diverted_idx
    ON gtfs_route (gtfs_id) WHERE active_variant_id IS NOT NULL;

-- ---------------------------------------------------------------- stops out of use
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS unserviceable boolean NOT NULL DEFAULT false;

-- the Stops out of use page, and "is this stop out of use" on a stop read
CREATE INDEX IF NOT EXISTS gtfs_stop_unserviceable_idx
    ON gtfs_stop (gtfs_id) WHERE unserviceable;

-- ---------------------------------------------------------------- triggers
-- 0023 defines this for every table that carries a row_version; it is restated
-- so this file stands alone, and must stay the same shape as 0023's.
CREATE OR REPLACE FUNCTION gtfs_touch_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF to_jsonb(NEW) ? 'row_version' THEN
        NEW.row_version := OLD.row_version + 1;
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS gtfs_stage_touch ON gtfs_stage;
CREATE TRIGGER gtfs_stage_touch BEFORE UPDATE ON gtfs_stage
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_route_stage_touch ON gtfs_route_stage;
CREATE TRIGGER gtfs_route_stage_touch BEFORE UPDATE ON gtfs_route_stage
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

-- ---------------------------------------------------------------- changes
-- Which entities a draft's change may name, and what it may do to them. The
-- list is every entity 0023 allows plus the three this file adds, and the ops
-- gain `activate` (which list a route wears). A change row whose entity or op
-- is not here is a typo or a stale client, and the table refuses it.
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
        'stage', 'route_stages', 'route_variant'));
ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_op_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_op_check
    CHECK (op IN ('create', 'update', 'delete', 'replace', 'merge', 'activate'));

COMMIT;
