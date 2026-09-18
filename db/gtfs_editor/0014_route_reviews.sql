-- Route reviews: the routes most worth an operator's time, queued for someone to
-- walk (docs/gtfs-editor.md section 14). Loaded by nandi's
-- `scripts/chennai-bus/editor/load_route_reviews.py`.
--
-- Coordinate reviews (0007) start from a defect and ask "is this stop wrong?".
-- This queue starts from the opposite end: a route nobody rides can be wrong for
-- years and cost nothing, while a defect on the busiest route is felt by
-- thousands of passengers a day. So the queue is ordered by USE - `queue_rank` 1
-- is the most used route - and `reasons` says what the load found wrong with it,
-- so an operator opening the queue sees both why this route matters and what to
-- look at when they get there.
--
-- `measure` names what was counted, because it will not always be the same
-- thing: a deployment with ticket sales should say so rather than quietly
-- passing off trips operated as bookings. `measure_value` is that measure's
-- number and `measure_window` the period it was counted over.
--
-- A fix is made with the ordinary route editor, as a `route` or `route_stops`
-- change in a draft; `/route-reviews/{id}/fix` only records which draft it went
-- into. Nothing here writes the feed.
--
--   pending    waiting for someone to walk it
--   approved   a draft holds a change to the route (change_set_id / change_id)
--   committed  the draft carrying that change was committed
--   confirmed  an operator walked it and the route is right as it stands
--   rejected   not worth fixing, or not a real problem
--   superseded a later load replaced it before anyone reviewed it
--
-- Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_route_review (
    review_id          bigserial   PRIMARY KEY,
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    batch              text        NOT NULL,
    route_id           text COLLATE "C" NOT NULL,
    route_short_name   text,                               -- the public number, as the feed spells it
    route_long_name    text,
    -- 1 is the most used route of the batch; the queue is ordered by it
    queue_rank         integer     NOT NULL CHECK (queue_rank > 0),
    -- what was counted, never assumed: 'bookings', 'trips_operated', ...
    measure            text        NOT NULL,
    measure_value      double precision NOT NULL,
    measure_window     text,                               -- the period it was counted over
    -- [{code, severity, message, ...}] - what the load found wrong with the route
    reasons            jsonb       NOT NULL DEFAULT '[]'::jsonb,
    -- {stop_count, served_stop_count, has_polyline, route_numbers, source, ...}
    evidence           jsonb       NOT NULL DEFAULT '{}'::jsonb,
    status             text        NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'approved', 'committed', 'confirmed',
                                         'rejected', 'superseded')),
    change_set_id      uuid        REFERENCES gtfs_change_set (change_set_id) ON DELETE SET NULL,
    change_id          bigint,
    reviewed_by        uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at        timestamptz,
    review_note        text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK ((status = 'approved') = (change_set_id IS NOT NULL)
           OR status IN ('committed', 'confirmed', 'rejected', 'superseded'))
);

-- the queue itself: worst-ranked first, within a status
CREATE INDEX IF NOT EXISTS gtfs_route_review_queue_idx
    ON gtfs_route_review (gtfs_id, status, queue_rank, review_id);
-- the route page asks "has this route been queued", in any status
CREATE INDEX IF NOT EXISTS gtfs_route_review_route_idx
    ON gtfs_route_review (gtfs_id, route_id, review_id);
CREATE INDEX IF NOT EXISTS gtfs_route_review_name_trgm
    ON gtfs_route_review USING gin (route_short_name gin_trgm_ops);
CREATE INDEX IF NOT EXISTS gtfs_route_review_change_set_idx
    ON gtfs_route_review (change_set_id) WHERE change_set_id IS NOT NULL;
-- the list filters on one reason code, and the summary counts every code
CREATE INDEX IF NOT EXISTS gtfs_route_review_reasons_idx
    ON gtfs_route_review USING gin (reasons jsonb_path_ops);
-- one open review per route: a reload supersedes the old one rather than queuing
-- the same route twice
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_route_review_open_uq
    ON gtfs_route_review (gtfs_id, route_id) WHERE status IN ('pending', 'approved');

DROP TRIGGER IF EXISTS gtfs_route_review_touch ON gtfs_route_review;
CREATE TRIGGER gtfs_route_review_touch BEFORE UPDATE ON gtfs_route_review
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

COMMIT;
