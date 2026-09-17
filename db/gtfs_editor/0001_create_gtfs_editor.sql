-- GTFS metadata editor: the editable source of truth for a feed's stops, routes,
-- route-stop order, polylines and stations, plus the draft -> approve -> commit
-- workflow, users and audit.
--
-- Lives in the internal DB beside the *_internal tables and never touches them:
-- route_internal / stop_internal / route_point_internal keep MTC's own ids for
-- the operator flows, while these tables hold the cleaned GTFS the feed ships.
--
-- Every data table is keyed by gtfs_id (the feed key, e.g. chennai_bus). Ids
-- use COLLATE "C" so ordering matches prod (C.UTF-8) instead of this server's
-- en_US.UTF8.

BEGIN;

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------- feeds
CREATE TABLE gtfs_feed (
    gtfs_id            text COLLATE "C" PRIMARY KEY,
    display_name       text        NOT NULL,
    -- bumped in the same transaction as every commit; GIMS pods poll it and
    -- reload a feed whose version moved
    version            bigint      NOT NULL DEFAULT 1,
    -- 'db' = GIMS and the nandi build read this feed from these tables
    data_source        text        NOT NULL DEFAULT 'preprocessed'
                       CHECK (data_source IN ('db', 'preprocessed')),
    agency_name        text,
    released_version   bigint,
    released_at        timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------- stops
-- location_type 0 = stop / platform, 1 = station. "Club stops into a station"
-- = a location_type 1 row plus parent_station on each member stop.
CREATE TABLE gtfs_stop (
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    stop_id            text COLLATE "C" NOT NULL,
    stop_code          text COLLATE "C",
    name               text        NOT NULL,
    lat                double precision NOT NULL CHECK (lat BETWEEN -90 AND 90),
    lon                double precision NOT NULL CHECK (lon BETWEEN -180 AND 180),
    location_type      smallint    NOT NULL DEFAULT 0 CHECK (location_type IN (0, 1)),
    parent_station     text COLLATE "C",
    platform_code      text,
    cluster_id         text COLLATE "C",
    regional_name      text,
    hindi_name         text,
    info_json          jsonb,
    -- where the position came from (REVIEW, SURVEY, CHALO, MAP, GOOGLE, CRM)
    -- and the cleanup's references (canonical id, human decisions)
    position_source    text,
    provenance         jsonb,
    deleted            boolean     NOT NULL DEFAULT false,
    row_version        integer     NOT NULL DEFAULT 1,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    PRIMARY KEY (gtfs_id, stop_id),
    FOREIGN KEY (gtfs_id, parent_station) REFERENCES gtfs_stop (gtfs_id, stop_id)
        DEFERRABLE INITIALLY DEFERRED,
    CHECK (parent_station IS NULL OR location_type = 0),
    CHECK (parent_station IS DISTINCT FROM stop_id)
);
CREATE INDEX gtfs_stop_parent_idx   ON gtfs_stop (gtfs_id, parent_station) WHERE parent_station IS NOT NULL;
CREATE INDEX gtfs_stop_cluster_idx  ON gtfs_stop (gtfs_id, cluster_id)     WHERE cluster_id IS NOT NULL;
CREATE INDEX gtfs_stop_latlon_idx   ON gtfs_stop (gtfs_id, lat, lon)       WHERE NOT deleted;
CREATE INDEX gtfs_stop_name_trgm    ON gtfs_stop USING gin (name gin_trgm_ops);
CREATE INDEX gtfs_stop_code_idx     ON gtfs_stop (gtfs_id, stop_code);

-- ---------------------------------------------------------------- routes
CREATE TABLE gtfs_route (
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    route_id           text COLLATE "C" NOT NULL,
    short_name         text,
    long_name          text,
    route_type         smallint    NOT NULL DEFAULT 3,      -- GTFS route_type, 3 = bus
    agency_id          text,
    color              text CHECK (color IS NULL OR color ~ '^#[0-9A-Fa-f]{6}$'),
    text_color         text CHECK (text_color IS NULL OR text_color ~ '^#[0-9A-Fa-f]{6}$'),
    encoded_polyline   text,
    polyline_source    text CHECK (polyline_source IS NULL OR polyline_source IN ('osrm', 'manual', 'imported')),
    service_type       text,
    provenance         jsonb,
    deleted            boolean     NOT NULL DEFAULT false,
    row_version        integer     NOT NULL DEFAULT 1,
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    PRIMARY KEY (gtfs_id, route_id)
);
CREATE INDEX gtfs_route_short_name_idx  ON gtfs_route (gtfs_id, short_name) WHERE NOT deleted;
CREATE INDEX gtfs_route_short_name_trgm ON gtfs_route USING gin (short_name gin_trgm_ops);
CREATE INDEX gtfs_route_long_name_trgm  ON gtfs_route USING gin (long_name gin_trgm_ops);

-- ---------------------------------------------------------------- route stop order
-- One row per position on a route, exactly the clean_route_mapping.csv row.
-- The fare invariant lives here: an INTERMEDIATE STOP carries the preceding
-- NEW STOP's stage_no / stage_name. ROUTE CORRECTION rows are polyline shaping
-- points, not stops: they carry their own marker id and position instead of a
-- stop_id.
CREATE TABLE gtfs_route_stop (
    gtfs_id            text COLLATE "C" NOT NULL,
    route_id           text COLLATE "C" NOT NULL,
    sequence           integer     NOT NULL CHECK (sequence > 0),
    stop_id            text COLLATE "C",
    stop_type          text        NOT NULL CHECK (stop_type IN
                       ('NEW STOP', 'INTERMEDIATE STOP', 'JUMP STOP', 'ROUTE CORRECTION', 'HIDDEN STOP')),
    stage_no           integer     NOT NULL,
    stage_name         text        NOT NULL,
    marker_id          text COLLATE "C",
    marker_lat         double precision,
    marker_lon         double precision,
    provider_id        text,
    provenance         jsonb,
    updated_at         timestamptz NOT NULL DEFAULT now(),
    updated_by         text,
    PRIMARY KEY (gtfs_id, route_id, sequence),
    FOREIGN KEY (gtfs_id, route_id) REFERENCES gtfs_route (gtfs_id, route_id) ON DELETE CASCADE,
    FOREIGN KEY (gtfs_id, stop_id)  REFERENCES gtfs_stop (gtfs_id, stop_id),
    CHECK ((stop_type = 'ROUTE CORRECTION') = (stop_id IS NULL)),
    CHECK (stop_type <> 'ROUTE CORRECTION' OR (marker_lat IS NOT NULL AND marker_lon IS NOT NULL))
);
CREATE INDEX gtfs_route_stop_stop_idx ON gtfs_route_stop (gtfs_id, stop_id) WHERE stop_id IS NOT NULL;

-- ---------------------------------------------------------------- people
CREATE TABLE gtfs_editor_user (
    user_id            uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email              text        NOT NULL,
    display_name       text,
    role               text        NOT NULL DEFAULT 'viewer'
                       CHECK (role IN ('viewer', 'editor', 'approver', 'admin')),
    -- TOTP secret, encrypted by the application with a key from its k8s secret;
    -- never stored or logged in clear
    totp_secret_enc    bytea,
    totp_enabled       boolean     NOT NULL DEFAULT false,
    totp_last_step     bigint,     -- rejects a replayed code within its 30 s step
    status             text        NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled')),
    created_at         timestamptz NOT NULL DEFAULT now(),
    last_login_at      timestamptz
);
CREATE UNIQUE INDEX gtfs_editor_user_email_idx ON gtfs_editor_user (lower(email));

CREATE TABLE gtfs_editor_session (
    token_hash         bytea       PRIMARY KEY,            -- sha256 of the cookie token
    user_id            uuid        NOT NULL REFERENCES gtfs_editor_user (user_id) ON DELETE CASCADE,
    created_at         timestamptz NOT NULL DEFAULT now(),
    expires_at         timestamptz NOT NULL,
    mfa_verified_at    timestamptz,
    last_seen_at       timestamptz,
    client_ip          inet,
    user_agent         text
);
CREATE INDEX gtfs_editor_session_user_idx ON gtfs_editor_session (user_id, expires_at);

-- ---------------------------------------------------------------- drafts
-- A change set is a shared draft. Any editor can add to a draft; submitting
-- freezes it; a DIFFERENT person approves; commit applies every change in one
-- transaction, checks each row_version against the draft's base, and bumps
-- gtfs_feed.version.
CREATE TABLE gtfs_change_set (
    change_set_id      uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    title              text        NOT NULL,
    description        text,
    status             text        NOT NULL DEFAULT 'draft'
                       CHECK (status IN ('draft', 'submitted', 'approved', 'rejected', 'committed', 'discarded')),
    created_by         uuid        NOT NULL REFERENCES gtfs_editor_user (user_id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    updated_at         timestamptz NOT NULL DEFAULT now(),
    submitted_by       uuid        REFERENCES gtfs_editor_user (user_id),
    submitted_at       timestamptz,
    reviewed_by        uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at        timestamptz,
    review_comment     text,
    committed_by       uuid        REFERENCES gtfs_editor_user (user_id),
    committed_at       timestamptz,
    base_version       bigint      NOT NULL,               -- feed version the draft started from
    committed_version  bigint,
    -- maker-checker: whoever submitted cannot approve
    CHECK (reviewed_by IS NULL OR reviewed_by IS DISTINCT FROM submitted_by)
);
CREATE INDEX gtfs_change_set_feed_status_idx ON gtfs_change_set (gtfs_id, status, updated_at DESC);

CREATE TABLE gtfs_change (
    change_id          bigserial   PRIMARY KEY,
    change_set_id      uuid        NOT NULL REFERENCES gtfs_change_set (change_set_id) ON DELETE CASCADE,
    position           integer     NOT NULL,               -- apply order within the set
    entity             text        NOT NULL CHECK (entity IN ('stop', 'route', 'route_stops', 'station')),
    entity_key         text COLLATE "C" NOT NULL,          -- stop_id or route_id
    op                 text        NOT NULL CHECK (op IN ('create', 'update', 'delete', 'replace')),
    base_row_version   integer,                            -- row_version when the edit was made
    before             jsonb,
    after              jsonb,
    created_by         uuid        NOT NULL REFERENCES gtfs_editor_user (user_id),
    created_at         timestamptz NOT NULL DEFAULT now(),
    UNIQUE (change_set_id, position)
);
CREATE INDEX gtfs_change_entity_idx ON gtfs_change (entity, entity_key);

-- ---------------------------------------------------------------- releases
CREATE TABLE gtfs_release (
    release_id         bigserial   PRIMARY KEY,
    gtfs_id            text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    feed_version       bigint      NOT NULL,
    status             text        NOT NULL CHECK (status IN ('building', 'built', 'released', 'failed', 'skipped')),
    zip_sha256         text,
    nandi_commit       text,
    detail             jsonb,
    created_at         timestamptz NOT NULL DEFAULT now(),
    finished_at        timestamptz
);
CREATE INDEX gtfs_release_feed_idx ON gtfs_release (gtfs_id, created_at DESC);

-- ---------------------------------------------------------------- audit
CREATE TABLE gtfs_audit_log (
    audit_id           bigserial   PRIMARY KEY,
    at                 timestamptz NOT NULL DEFAULT now(),
    actor              uuid        REFERENCES gtfs_editor_user (user_id),
    actor_email        text,
    action             text        NOT NULL,
    gtfs_id            text COLLATE "C",
    change_set_id      uuid,
    detail             jsonb
);
CREATE INDEX gtfs_audit_log_feed_idx ON gtfs_audit_log (gtfs_id, at DESC);
CREATE INDEX gtfs_audit_log_change_set_idx ON gtfs_audit_log (change_set_id) WHERE change_set_id IS NOT NULL;

-- ---------------------------------------------------------------- triggers
CREATE FUNCTION gtfs_touch_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_TABLE_NAME IN ('gtfs_stop', 'gtfs_route') THEN
        -- optimistic locking for drafts: a commit whose base row_version no
        -- longer matches is a conflict, not an overwrite
        NEW.row_version := OLD.row_version + 1;
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END $$;
CREATE TRIGGER gtfs_stop_touch        BEFORE UPDATE ON gtfs_stop        FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
CREATE TRIGGER gtfs_route_touch       BEFORE UPDATE ON gtfs_route       FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
CREATE TRIGGER gtfs_route_stop_touch  BEFORE UPDATE ON gtfs_route_stop  FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
CREATE TRIGGER gtfs_feed_touch        BEFORE UPDATE ON gtfs_feed        FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
CREATE TRIGGER gtfs_change_set_touch  BEFORE UPDATE ON gtfs_change_set  FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

-- the audit log is append-only
CREATE FUNCTION gtfs_audit_log_immutable() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'gtfs_audit_log is append-only';
END $$;
CREATE TRIGGER gtfs_audit_log_no_update BEFORE UPDATE OR DELETE ON gtfs_audit_log
    FOR EACH ROW EXECUTE FUNCTION gtfs_audit_log_immutable();

COMMIT;
