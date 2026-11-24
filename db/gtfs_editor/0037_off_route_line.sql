-- A stop a route's map line passes by: "the bus may not stop here"
-- (docs/gtfs-editor.md section 27, 2026-11-24).
--
-- A line drawn from GPS follows where the buses went; where they take a flyover
-- past a stop on the service road, the line passes the stop by. GIMS measures
-- each bus route's line against its stops once per feed version, on the version
-- poll of one pod (services::off_route_line), and keeps what it found here, so
-- the pods that serve only read it:
--
--   gtfs_off_route_line      one row per (route, stop) the line passes more than
--                            threshold_m from, and how far
--   gtfs_off_route_line_run  per feed: the version it was measured on, and a
--                            revision that moves only when the flagged stops do -
--                            what each pod's version poll watches
--
-- Both are derived from committed rows and written by that measurement alone,
-- never by a draft. Neither has a row_version or a touch trigger, and writing them does not
-- move gtfs_feed.version: storing the flags must not reload every pod's feed or
-- fire a release. Keys are what GIMS serves: the route id, and the stop's code
-- (stop_code, else stop_id).
--
-- Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_off_route_line_run (
    gtfs_id         text COLLATE "C" PRIMARY KEY REFERENCES gtfs_feed (gtfs_id) ON DELETE CASCADE,
    -- the feed version the flags were measured on; a pod serving an older one
    -- never measures over them
    feed_version    bigint           NOT NULL,
    -- the distance a stop had to be from the line to be flagged; a change of it
    -- in the code measures every feed again
    threshold_m     double precision NOT NULL,
    -- moves only when the set of flagged stops changes; pods re-read on it
    revision        bigint           NOT NULL DEFAULT 1,
    routes_checked  integer          NOT NULL,
    stops_flagged   integer          NOT NULL,
    took_ms         bigint,
    refreshed_at    timestamptz      NOT NULL DEFAULT now(),
    -- the pod that measured
    refreshed_by    text
);

CREATE TABLE IF NOT EXISTS gtfs_off_route_line (
    gtfs_id         text COLLATE "C" NOT NULL
                    REFERENCES gtfs_off_route_line_run (gtfs_id) ON DELETE CASCADE,
    route_id        text COLLATE "C" NOT NULL,
    stop_code       text COLLATE "C" NOT NULL,
    -- how far the line passes from the stop
    distance_m      double precision NOT NULL,
    PRIMARY KEY (gtfs_id, route_id, stop_code)
);

COMMIT;
