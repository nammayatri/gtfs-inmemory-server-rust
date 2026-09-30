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
    -- Which way along the corridor this stage runs, and PART OF ITS KEY.
    -- A stage is identified by the stop it begins at - MTC's bus_stop_id - and
    -- the direction: 1,746 of MTC's 1,836 fare-stage stops are used both ways,
    -- and the stops after the boundary differ, so the up stage and the down
    -- stage of one corridor are two stages sharing an id.
    -- '' is a stage that runs the same either way; the API shows it as null.
    direction          text        NOT NULL DEFAULT ''
                       CHECK (direction IN ('up', 'down', '')),
    -- a note for the people editing, to tell same-named stages apart further:
    -- "towards Guindy", "via the flyover"
    description        text,
    -- Why somebody still has to look at this stage, or NULL when nobody does.
    -- The routes sharing a fare stage do not always agree about its stops, and
    -- the backfill can only take the list most of them give. This says why the
    -- guess may be wrong; the detail - every list the routes gave, and which
    -- routes gave it - is in gtfs_stage_review. Cleared when that review is
    -- closed, by an ordinary stage/update in a draft, so who cleared it and
    -- when is audited like any other edit.
    --
    --   head_differs          the routes begin it at stops with different names
    --   head_duplicate_stops  at different stop records carrying one name
    --   stretch_differs       same beginning, different stops after it
    --
    -- Left free text rather than a CHECK, as gtfs_position_review.reason is, so
    -- a new reason does not need a migration.
    review             text,
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
    PRIMARY KEY (gtfs_id, stage_id, direction)
);
-- for a database built by an earlier run of this file, when the table already
-- exists and CREATE TABLE IF NOT EXISTS is a no-op
ALTER TABLE gtfs_stage ADD COLUMN IF NOT EXISTS direction text;
ALTER TABLE gtfs_stage ADD COLUMN IF NOT EXISTS review text;
-- direction is part of the key, so it cannot be NULL: '' is "runs either way".
-- The old CHECK goes first, or it refuses the '' this writes.
ALTER TABLE gtfs_stage DROP CONSTRAINT IF EXISTS gtfs_stage_direction_check;
UPDATE gtfs_stage SET direction = '' WHERE direction IS NULL;
ALTER TABLE gtfs_stage ALTER COLUMN direction SET DEFAULT '';
ALTER TABLE gtfs_stage ALTER COLUMN direction SET NOT NULL;
ALTER TABLE gtfs_stage ADD CONSTRAINT gtfs_stage_direction_check
    CHECK (direction IN ('up', 'down', ''));

CREATE INDEX IF NOT EXISTS gtfs_stage_name_trgm ON gtfs_stage USING gin (name gin_trgm_ops);
-- the reviewer's list: the stages still to look at
CREATE INDEX IF NOT EXISTS gtfs_stage_review_idx ON gtfs_stage (gtfs_id, review, name)
    WHERE review IS NOT NULL AND NOT deleted;

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
    -- part of the stage's key, so a row says which of the two stages it is on
    direction          text        NOT NULL DEFAULT '',
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
    PRIMARY KEY (gtfs_id, stage_id, direction, position),
    FOREIGN KEY (gtfs_id, stage_id, direction)
        REFERENCES gtfs_stage (gtfs_id, stage_id, direction) ON DELETE CASCADE,
    FOREIGN KEY (gtfs_id, stop_id)  REFERENCES gtfs_stop (gtfs_id, stop_id),
    CHECK ((stop_type = 'ROUTE CORRECTION') = (stop_id IS NULL)),
    CHECK (stop_type <> 'ROUTE CORRECTION' OR (marker_lat IS NOT NULL AND marker_lon IS NOT NULL)),
    CHECK (stop_name_override IS NULL OR stop_id IS NOT NULL)
);
-- for a database built by an earlier run of this file
ALTER TABLE gtfs_stage_stop ADD COLUMN IF NOT EXISTS direction text NOT NULL DEFAULT '';
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
    -- part of the stage's key; see gtfs_stage.direction
    direction          text        NOT NULL DEFAULT '',
    stage_no           integer     NOT NULL,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    FOREIGN KEY (gtfs_id, route_id) REFERENCES gtfs_route (gtfs_id, route_id) ON DELETE CASCADE,
    FOREIGN KEY (gtfs_id, stage_id, direction)
        REFERENCES gtfs_stage (gtfs_id, stage_id, direction)
);
-- the key over (list, position). It is a unique index and not a primary key
-- because a primary key cannot hold NULL, and the normal list's variant_id
-- genuinely is NULL.
ALTER TABLE gtfs_route_stage ADD COLUMN IF NOT EXISTS direction text NOT NULL DEFAULT '';
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_route_stage_key
    ON gtfs_route_stage (gtfs_id, route_id, COALESCE(variant_id, ''), position);
CREATE INDEX IF NOT EXISTS gtfs_route_stage_stage_idx
    ON gtfs_route_stage (gtfs_id, stage_id, direction);

-- ------------------------------------------- moving the key on an existing database
-- CREATE TABLE IF NOT EXISTS does nothing to a table that is already there, so a
-- database built before direction joined the stage's key still has the old one.
-- The foreign keys pointing at it go first, then the keys, then the foreign keys
-- come back naming direction too. Skipped when the key is already right.
DO $$
DECLARE
    already boolean;
BEGIN
    SELECT EXISTS (
        SELECT 1 FROM pg_constraint c
        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)
        WHERE c.conname = 'gtfs_stage_pkey' AND a.attname = 'direction'
    ) INTO already;
    IF already THEN
        RETURN;
    END IF;
    ALTER TABLE gtfs_stage_stop  DROP CONSTRAINT IF EXISTS gtfs_stage_stop_gtfs_id_stage_id_fkey;
    ALTER TABLE gtfs_route_stage DROP CONSTRAINT IF EXISTS gtfs_route_stage_gtfs_id_stage_id_fkey;
    ALTER TABLE gtfs_stage_stop  DROP CONSTRAINT IF EXISTS gtfs_stage_stop_pkey;
    ALTER TABLE gtfs_stage       DROP CONSTRAINT IF EXISTS gtfs_stage_pkey;
    ALTER TABLE gtfs_stage       ADD PRIMARY KEY (gtfs_id, stage_id, direction);
    ALTER TABLE gtfs_stage_stop  ADD PRIMARY KEY (gtfs_id, stage_id, direction, position);
    ALTER TABLE gtfs_stage_stop
        ADD CONSTRAINT gtfs_stage_stop_gtfs_id_stage_id_direction_fkey
        FOREIGN KEY (gtfs_id, stage_id, direction)
        REFERENCES gtfs_stage (gtfs_id, stage_id, direction) ON DELETE CASCADE;
    ALTER TABLE gtfs_route_stage
        ADD CONSTRAINT gtfs_route_stage_gtfs_id_stage_id_direction_fkey
        FOREIGN KEY (gtfs_id, stage_id, direction)
        REFERENCES gtfs_stage (gtfs_id, stage_id, direction);
END $$;

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

-- --------------------------------------------------------- stage reviews
-- The queue the operations team works through, as gtfs_position_review
-- (section 8) is for stops whose position is suspected wrong.
--
-- One row per stage the routes disagree about. The backfill takes the stop list
-- most routes give a stage and raises this row carrying every list they gave,
-- so a person can build the real one in the dashboard; that goes into a draft
-- like any other edit, and committing it is what updates the stop mapping.
--
-- Closing a row clears gtfs_stage.review on its stage, in the same transaction,
-- so the flag and the queue cannot drift. Nothing is ever deleted: a closed row
-- is the record of a decision, and a later backfill supersedes only the rows
-- nobody has closed.
CREATE TABLE IF NOT EXISTS gtfs_stage_review (
    review_id     bigserial PRIMARY KEY,
    gtfs_id       text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    -- which run raised it, so a later run can supersede a whole batch
    batch         text        NOT NULL,
    -- the stage under review: what to show, and what to find it by
    name          text        NOT NULL,
    name_key      text        NOT NULL,
    direction     text        NOT NULL DEFAULT '' CHECK (direction IN ('up', 'down', '')),
    -- the same vocabulary as gtfs_stage.review; see the comment there
    reason        text        NOT NULL,
    -- How many stop calls the backfill's guess gets wrong: for every route that
    -- does not have the chosen list, the stops it gains plus the stops it loses.
    -- The queue is worked in this order. It is very unevenly spread - on
    -- chennai_bus the top 776 of 2,037 rows hold 91% of the difference - so this
    -- is what makes the queue finishable.
    impact        integer     NOT NULL DEFAULT 0,
    -- every list the routes gave, each with its stops and the routes giving it,
    -- under "candidates": what the dashboard puts in front of the reviewer
    evidence      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- pending    nobody has looked
    -- fixed      looked at and put right, usually in the draft named below
    -- confirmed  looked at, nothing to change: the routes really do differ
    -- superseded a later batch raised a fresh row for this stage
    status        text        NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'fixed', 'confirmed', 'superseded')),
    change_set_id uuid        REFERENCES gtfs_change_set (change_set_id) ON DELETE SET NULL,
    reviewed_by   uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at   timestamptz,
    review_note   text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE gtfs_stage_review ADD COLUMN IF NOT EXISTS impact integer NOT NULL DEFAULT 0;
-- for a database built by an earlier run: direction is '' rather than NULL here
-- too, so a review names its stage by the same pair the stage's key uses. The
-- old CHECK goes first, or it refuses the '' this writes.
ALTER TABLE gtfs_stage_review DROP CONSTRAINT IF EXISTS gtfs_stage_review_direction_check;
UPDATE gtfs_stage_review SET direction = '' WHERE direction IS NULL;
ALTER TABLE gtfs_stage_review ALTER COLUMN direction SET DEFAULT '';
ALTER TABLE gtfs_stage_review ALTER COLUMN direction SET NOT NULL;
ALTER TABLE gtfs_stage_review ADD CONSTRAINT gtfs_stage_review_direction_check
    CHECK (direction IN ('up', 'down', ''));
-- one open row per stage; a closed one does not block the next
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_stage_review_open_key
    ON gtfs_stage_review (gtfs_id, name_key, direction)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS gtfs_stage_review_list_idx
    ON gtfs_stage_review (gtfs_id, status, impact DESC, reason, name);
CREATE INDEX IF NOT EXISTS gtfs_stage_review_set_idx
    ON gtfs_stage_review (change_set_id) WHERE change_set_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS gtfs_stage_review_name_trgm
    ON gtfs_stage_review USING gin (name gin_trgm_ops);
DROP TRIGGER IF EXISTS gtfs_stage_review_touch ON gtfs_stage_review;
CREATE TRIGGER gtfs_stage_review_touch BEFORE UPDATE ON gtfs_stage_review
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

-- ------------------------------------------------- the mapping, derived
-- Where a route's stops come from, said once in SQL: the route's stages, in
-- their order, each stage's stops in theirs, for whichever list the route is
-- wearing (its normal one, or the temporary route it is running).
--
-- `gtfs_route_stop` holds the same rows and is what every reader uses - the
-- GTFS export, the in-memory loader, nandi's build, merges, reviews. It is not
-- a second source of truth: a stage edit rewrites it, and refuses a route where
-- the two have come apart (`route_out_of_sync`). This view is how that is
-- checked, and the answer to "where does the route-to-stop mapping come from".
--
-- It cannot replace the table. `stop_times.txt` carries ten per-stop fields -
-- pickup_type, drop_off_type, timepoint, shape_dist_traveled, stop_headsign,
-- the booking rules - that a stage has nowhere to put, and a route not built
-- from stages has no rows here at all. Both live in gtfs_route_stop.
CREATE OR REPLACE VIEW gtfs_route_stop_from_stages AS
SELECT rs.gtfs_id,
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
       ss.marker_lon
  FROM gtfs_route_stage rs
  JOIN gtfs_route r ON r.gtfs_id = rs.gtfs_id AND r.route_id = rs.route_id AND NOT r.deleted
  JOIN gtfs_stage st ON st.gtfs_id = rs.gtfs_id AND st.stage_id = rs.stage_id
                    AND st.direction = rs.direction
  JOIN gtfs_stage_stop ss ON ss.gtfs_id = rs.gtfs_id AND ss.stage_id = rs.stage_id
                         AND ss.direction = rs.direction
 WHERE rs.variant_id IS NOT DISTINCT FROM r.active_variant_id;

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
