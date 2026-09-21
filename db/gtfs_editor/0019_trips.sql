-- Trips, stop times and service calendars in the editor's tables
-- (docs/gtfs-editor.md section 16).
--
-- Until now a feed's trips and their times came from the preprocessed data
-- only: the tables held a route's one stop order and the editor could not touch
-- when anything runs. This moves the rest of the timetable in:
--
--   gtfs_pattern          a route's stop orders. Pattern 1 is the stop list every
--                         route already has in gtfs_route_stop; metro and
--                         suburban routes add more (short turns, express runs).
--                         Its public id is computed, never stored.
--   gtfs_timing_profile   arrival / departure offsets per served stop of one
--                         pattern, shared by every trip that runs to it. A trip
--                         with none runs to the pattern's default timing, which
--                         is not stored either (the feed's default_run_s and
--                         default_dwell_s, the generator's formula).
--   gtfs_service          the days a trip runs, with gtfs_service_date for the
--                         dates added or removed.
--   gtfs_trip             a pattern, a profile or the default, a service, a
--                         direction and a reference time. Its stop times are the
--                         reference time plus the profile's offsets: nothing per
--                         stop per trip is stored.
--   gtfs_frequency        headway windows of a frequency-based trip.
--   gtfs_sync_run         one row per run of the MTC schedule sync (16.8).
--
-- Every row gtfs_route_stop holds today becomes pattern 1, and code that takes
-- no pattern keeps meaning pattern 1. The primary key gains the pattern.
--
-- Pattern 1 exists for every route: the backfill below creates it for the
-- routes there are, and a trigger for every route inserted afterwards, whoever
-- inserts it - the editor's route/create, or a seeder writing rows directly. A
-- route's stop rows reference their pattern, so a pattern's rows go with it.
--
-- gtfs_feed.trips_source says where GIMS takes a feed's trips from, independently
-- of its data_source; 'preprocessed' (the default) is today's behaviour exactly.
--
-- Safe to run twice.

BEGIN;

-- ------------------------------------------------------------ route stop rows
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS pattern_key smallint NOT NULL DEFAULT 1
    CHECK (pattern_key > 0);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS pickup_type   smallint CHECK (pickup_type   BETWEEN 0 AND 3);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS drop_off_type smallint CHECK (drop_off_type BETWEEN 0 AND 3);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS timepoint     smallint CHECK (timepoint IN (0, 1));

-- the primary key becomes (gtfs_id, route_id, pattern_key, sequence); rebuilt
-- only when it does not have the pattern yet
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint c
         WHERE c.conrelid = 'gtfs_route_stop'::regclass AND c.contype = 'p'
           AND pg_get_constraintdef(c.oid) = 'PRIMARY KEY (gtfs_id, route_id, pattern_key, sequence)'
    ) THEN
        ALTER TABLE gtfs_route_stop DROP CONSTRAINT IF EXISTS gtfs_route_stop_pkey;
        ALTER TABLE gtfs_route_stop ADD CONSTRAINT gtfs_route_stop_pkey
            PRIMARY KEY (gtfs_id, route_id, pattern_key, sequence);
    END IF;
END $$;

-- ------------------------------------------------------------ patterns
CREATE TABLE IF NOT EXISTS gtfs_pattern (
    gtfs_id       text COLLATE "C" NOT NULL,
    route_id      text COLLATE "C" NOT NULL,
    pattern_key   smallint    NOT NULL CHECK (pattern_key > 0),
    name          text,                       -- ops' label: "short turn to Majestic"
    direction_id  smallint    CHECK (direction_id IN (0, 1)),
    row_version   integer     NOT NULL DEFAULT 1,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text,
    PRIMARY KEY (gtfs_id, route_id, pattern_key),
    FOREIGN KEY (gtfs_id, route_id) REFERENCES gtfs_route (gtfs_id, route_id) ON DELETE CASCADE
);

-- pattern 1 for every route there is
INSERT INTO gtfs_pattern (gtfs_id, route_id, pattern_key)
SELECT gtfs_id, route_id, 1 FROM gtfs_route
ON CONFLICT DO NOTHING;
-- and for any other stop order rows already name (none, the first time)
INSERT INTO gtfs_pattern (gtfs_id, route_id, pattern_key)
SELECT DISTINCT gtfs_id, route_id, pattern_key FROM gtfs_route_stop
ON CONFLICT DO NOTHING;

-- and for every route inserted from now on, by anything
CREATE OR REPLACE FUNCTION gtfs_route_pattern_one() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    INSERT INTO gtfs_pattern (gtfs_id, route_id, pattern_key, updated_by)
    VALUES (NEW.gtfs_id, NEW.route_id, 1, NEW.updated_by)
    ON CONFLICT DO NOTHING;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS gtfs_route_pattern_one ON gtfs_route;
CREATE TRIGGER gtfs_route_pattern_one AFTER INSERT ON gtfs_route
    FOR EACH ROW EXECUTE FUNCTION gtfs_route_pattern_one();

-- a stop row belongs to its pattern, and goes with it
ALTER TABLE gtfs_route_stop DROP CONSTRAINT IF EXISTS gtfs_route_stop_pattern_fkey;
ALTER TABLE gtfs_route_stop ADD CONSTRAINT gtfs_route_stop_pattern_fkey
    FOREIGN KEY (gtfs_id, route_id, pattern_key)
    REFERENCES gtfs_pattern (gtfs_id, route_id, pattern_key) ON DELETE CASCADE;

-- ------------------------------------------------------------ timing profiles
CREATE TABLE IF NOT EXISTS gtfs_timing_profile (
    gtfs_id       text COLLATE "C" NOT NULL,
    route_id      text COLLATE "C" NOT NULL,
    pattern_key   smallint    NOT NULL,
    profile_key   integer     NOT NULL CHECK (profile_key > 0),
    arrival_s     integer[]   NOT NULL,       -- one per served stop, in sequence order
    departure_s   integer[]   NOT NULL,
    label         text,                       -- "peak", "MTC 70 min"
    source        text        NOT NULL CHECK (source IN
                  ('import', 'mtc_running_time', 'manual', 'interpolated')),
    row_version   integer     NOT NULL DEFAULT 1,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text,
    PRIMARY KEY (gtfs_id, route_id, pattern_key, profile_key),
    FOREIGN KEY (gtfs_id, route_id, pattern_key)
        REFERENCES gtfs_pattern (gtfs_id, route_id, pattern_key) ON DELETE CASCADE,
    CHECK (cardinality(arrival_s) = cardinality(departure_s) AND cardinality(arrival_s) >= 2)
);

-- ------------------------------------------------------------ service calendars
CREATE TABLE IF NOT EXISTS gtfs_service (
    gtfs_id     text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    service_id  text COLLATE "C" NOT NULL,
    monday boolean NOT NULL, tuesday boolean NOT NULL, wednesday boolean NOT NULL,
    thursday boolean NOT NULL, friday boolean NOT NULL, saturday boolean NOT NULL,
    sunday boolean NOT NULL,
    start_date  date,                         -- both NULL: a calendar_dates-only service
    end_date    date,
    label       text,
    row_version integer     NOT NULL DEFAULT 1,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text,
    PRIMARY KEY (gtfs_id, service_id),
    CHECK ((start_date IS NULL) = (end_date IS NULL)),
    CHECK (end_date >= start_date)
);

CREATE TABLE IF NOT EXISTS gtfs_service_date (
    gtfs_id        text COLLATE "C" NOT NULL,
    service_id     text COLLATE "C" NOT NULL,
    service_date   date     NOT NULL,
    exception_type smallint NOT NULL CHECK (exception_type IN (1, 2)),
    PRIMARY KEY (gtfs_id, service_id, service_date),
    FOREIGN KEY (gtfs_id, service_id) REFERENCES gtfs_service (gtfs_id, service_id) ON DELETE CASCADE
);

-- ------------------------------------------------------------ trips
CREATE TABLE IF NOT EXISTS gtfs_trip (
    gtfs_id       text COLLATE "C" NOT NULL,
    trip_id       text COLLATE "C" NOT NULL,
    route_id      text COLLATE "C" NOT NULL,
    pattern_key   smallint    NOT NULL,
    profile_key   integer,                    -- NULL: the pattern's default timing
    service_id    text COLLATE "C" NOT NULL,
    direction_id  smallint    CHECK (direction_id IN (0, 1)),
    ref_s         integer     NOT NULL CHECK (ref_s BETWEEN 0 AND 172799),
    headsign      text,
    short_name    text,
    block_id      text,
    shape_id      text,                       -- GTFS shape_id, as the feed has it (no shapes table yet)
    wheelchair_accessible smallint,
    bikes_allowed smallint,
    sort_key      integer     NOT NULL,       -- source order; a route's example trip is its first
    source        text        NOT NULL CHECK (source IN ('import', 'mtc', 'editor')),
    source_ref    jsonb,                      -- MTC: {schedule_trip_detail_id, schedule_number, service_type_code}
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text,
    PRIMARY KEY (gtfs_id, trip_id),
    FOREIGN KEY (gtfs_id, route_id, pattern_key)
        REFERENCES gtfs_pattern (gtfs_id, route_id, pattern_key),
    FOREIGN KEY (gtfs_id, route_id, pattern_key, profile_key)
        REFERENCES gtfs_timing_profile (gtfs_id, route_id, pattern_key, profile_key),
    FOREIGN KEY (gtfs_id, service_id) REFERENCES gtfs_service (gtfs_id, service_id)
);
-- a table created before shape_id was part of it
ALTER TABLE gtfs_trip ADD COLUMN IF NOT EXISTS shape_id text;
CREATE INDEX IF NOT EXISTS gtfs_trip_route_idx ON gtfs_trip (gtfs_id, route_id, pattern_key, sort_key);

CREATE TABLE IF NOT EXISTS gtfs_frequency (
    gtfs_id      text COLLATE "C" NOT NULL,
    trip_id      text COLLATE "C" NOT NULL,
    start_s      integer  NOT NULL,
    end_s        integer  NOT NULL CHECK (end_s > start_s),
    headway_s    integer  NOT NULL CHECK (headway_s > 0),
    exact_times  smallint NOT NULL DEFAULT 0 CHECK (exact_times IN (0, 1)),
    PRIMARY KEY (gtfs_id, trip_id, start_s),
    FOREIGN KEY (gtfs_id, trip_id) REFERENCES gtfs_trip (gtfs_id, trip_id) ON DELETE CASCADE
);

-- ------------------------------------------------------------ feed and route settings
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS trips_source       text    NOT NULL DEFAULT 'preprocessed'
    CHECK (trips_source IN ('preprocessed', 'db'));
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS default_run_s      integer NOT NULL DEFAULT 120;
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS default_dwell_s    integer NOT NULL DEFAULT 15;
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS schedule_sync      text    NOT NULL DEFAULT 'none'
    CHECK (schedule_sync IN ('none', 'mtc'));
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS sync_running_times boolean NOT NULL DEFAULT false;
ALTER TABLE gtfs_feed DROP CONSTRAINT IF EXISTS gtfs_feed_trips_need_db;
ALTER TABLE gtfs_feed ADD CONSTRAINT gtfs_feed_trips_need_db
    CHECK (trips_source = 'preprocessed' OR data_source = 'db');

ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS schedule_source text NOT NULL DEFAULT 'sync'
    CHECK (schedule_source IN ('sync', 'editor'));

-- ------------------------------------------------------------ the MTC sync's runs
CREATE TABLE IF NOT EXISTS gtfs_sync_run (
    run_id        uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    gtfs_id       text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    run_key       text        NOT NULL,       -- "2026-09-22" for the daily run; "manual:<uuid>"
    started_at    timestamptz NOT NULL DEFAULT now(),
    finished_at   timestamptz,
    status        text        NOT NULL CHECK (status IN ('running', 'no_change', 'drafted', 'failed')),
    change_set_id uuid        REFERENCES gtfs_change_set (change_set_id),
    summary       jsonb,
    UNIQUE (gtfs_id, run_key)
);

-- ------------------------------------------------------------ versions and drafts
-- A pattern, a timing profile and a service are edited through drafts like a
-- stop or a route, so they carry a row_version a commit checks the same way.
CREATE OR REPLACE FUNCTION gtfs_touch_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME IN ('gtfs_stop', 'gtfs_route', 'gtfs_pattern', 'gtfs_timing_profile',
                         'gtfs_service') THEN
        -- optimistic locking for drafts: a commit whose base row_version no
        -- longer matches is a conflict, not an overwrite
        NEW.row_version := OLD.row_version + 1;
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS gtfs_pattern_touch ON gtfs_pattern;
CREATE TRIGGER gtfs_pattern_touch BEFORE UPDATE ON gtfs_pattern
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_timing_profile_touch ON gtfs_timing_profile;
CREATE TRIGGER gtfs_timing_profile_touch BEFORE UPDATE ON gtfs_timing_profile
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_service_touch ON gtfs_service;
CREATE TRIGGER gtfs_service_touch BEFORE UPDATE ON gtfs_service
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_trip_touch ON gtfs_trip;
CREATE TRIGGER gtfs_trip_touch BEFORE UPDATE ON gtfs_trip
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_entity_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_entity_check
    CHECK (entity IN ('stop', 'route', 'route_stops', 'station', 'feed_config',
                      'pattern', 'timing_profile', 'route_trips', 'service'));

COMMIT;
