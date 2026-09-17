-- Position reviews: stops whose coordinate is suspected wrong (nandi's
-- src/assets/review/coordinate_suspects.csv - judged by agents, or found off
-- their own routes), queued for a person to check in the dashboard.
--
-- A reviewer either MOVES the stop - a `stop/update` change with the new lat/lon
-- goes into a draft, which someone else approves and commits - or CONFIRMS the
-- position is right, which closes the review without changing anything.
--
--   pending    waiting for review
--   approved   a move is in a draft (change_set_id / change_id)
--   committed  the draft carrying the move was committed
--   confirmed  a reviewer said the position is correct (no change)
--   superseded a later load replaced it before anyone reviewed it

BEGIN;

CREATE TABLE gtfs_position_review (
    review_id          bigserial   PRIMARY KEY,
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    batch              text        NOT NULL,
    stop_id            text COLLATE "C" NOT NULL,          -- the stop to check (current id)
    original_stop_id   text COLLATE "C" NOT NULL,          -- id in the suspects file (merged ids differ)
    stop_name          text        NOT NULL,
    reason             text        NOT NULL,               -- why it is suspected, in words
    lat                double precision NOT NULL,          -- position when loaded
    lon                double precision NOT NULL,
    raw_lat            double precision,                   -- the MTC master's own coordinate
    raw_lon            double precision,
    suggested_lat      double precision,                   -- a candidate, never applied on its own
    suggested_lon      double precision,
    suggested_source   text,                               -- e.g. "google: Sivan Temple"
    -- {route_rows, route_numbers, shares_point_with: [{stop_id, name}], chalo_nearby,
    --  detour_m, map_url}
    evidence           jsonb       NOT NULL DEFAULT '{}'::jsonb,
    status             text        NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'approved', 'committed', 'confirmed', 'superseded')),
    change_set_id      uuid        REFERENCES gtfs_change_set (change_set_id) ON DELETE SET NULL,
    change_id          bigint,
    reviewed_by        uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at        timestamptz,
    review_note        text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK ((suggested_lat IS NULL) = (suggested_lon IS NULL)),
    CHECK ((status = 'approved') = (change_set_id IS NOT NULL) OR status IN ('committed', 'confirmed', 'superseded'))
);
CREATE INDEX gtfs_position_review_status_idx ON gtfs_position_review (gtfs_id, status, review_id);
CREATE INDEX gtfs_position_review_latlon_idx ON gtfs_position_review (gtfs_id, lat, lon) WHERE status IN ('pending', 'approved');
CREATE INDEX gtfs_position_review_name_trgm ON gtfs_position_review USING gin (stop_name gin_trgm_ops);
CREATE INDEX gtfs_position_review_change_set_idx ON gtfs_position_review (change_set_id) WHERE change_set_id IS NOT NULL;
CREATE UNIQUE INDEX gtfs_position_review_open_uq ON gtfs_position_review (gtfs_id, stop_id)
    WHERE status IN ('pending', 'approved');

CREATE TRIGGER gtfs_position_review_touch BEFORE UPDATE ON gtfs_position_review
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

COMMIT;
