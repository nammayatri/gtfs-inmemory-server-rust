-- Station proposals: stations suggested by nandi's editor/build_stations.py
-- (same-named stops within 500 m), queued for a person to review. Nothing here
-- is live. Approving a proposal adds a `station/create` change to a draft; the
-- draft then goes through submit -> approve (someone else) -> commit like any
-- other edit, and only the commit creates the station.
--
--   pending   waiting for review
--   approved  in a draft (change_set_id / change_id say which)
--   rejected  a reviewer said no (review_note says why)
--   committed the draft that carried it was committed
--   superseded a later build replaced it before anyone reviewed it

BEGIN;

CREATE TABLE gtfs_station_proposal (
    proposal_id        bigserial   PRIMARY KEY,
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    batch              text        NOT NULL,               -- which build produced it
    station_id         text COLLATE "C" NOT NULL,          -- id the station would get
    name               text        NOT NULL,
    lat                double precision NOT NULL,
    lon                double precision NOT NULL,
    -- [{stop_id, name, lat, lon, platform_code, route_count}]
    members            jsonb       NOT NULL,
    spread_m           integer,
    status             text        NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'approved', 'rejected', 'committed', 'superseded')),
    change_set_id      uuid        REFERENCES gtfs_change_set (change_set_id) ON DELETE SET NULL,
    change_id          bigint,
    reviewed_by        uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at        timestamptz,
    review_note        text,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    CHECK (jsonb_typeof(members) = 'array'),
    CHECK ((status = 'approved') = (change_set_id IS NOT NULL) OR status IN ('committed', 'rejected', 'superseded'))
);
CREATE INDEX gtfs_station_proposal_status_idx ON gtfs_station_proposal (gtfs_id, status, proposal_id);
CREATE INDEX gtfs_station_proposal_latlon_idx ON gtfs_station_proposal (gtfs_id, lat, lon) WHERE status IN ('pending', 'approved');
CREATE INDEX gtfs_station_proposal_name_trgm ON gtfs_station_proposal USING gin (name gin_trgm_ops);
CREATE INDEX gtfs_station_proposal_change_set_idx ON gtfs_station_proposal (change_set_id) WHERE change_set_id IS NOT NULL;
-- one open proposal per would-be station id
CREATE UNIQUE INDEX gtfs_station_proposal_open_uq ON gtfs_station_proposal (gtfs_id, station_id)
    WHERE status IN ('pending', 'approved');

CREATE TRIGGER gtfs_station_proposal_touch BEFORE UPDATE ON gtfs_station_proposal
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

COMMIT;
