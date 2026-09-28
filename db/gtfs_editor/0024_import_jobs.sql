-- Imports as background jobs (docs/gtfs-editor.md section 18.15). A GTFS zip
-- import can run longer than the load balancer and Pomerium in front of the
-- editor wait (30 s each), so the dashboard asks for one in the background: the
-- pod that takes the request answers at once with a job, runs the import, and
-- writes its report here; the page polls for it, on whichever pod it lands.
--
-- Additive; the image before it never reads it. Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_import_job (
    job_id       uuid        PRIMARY KEY,
    -- the feed, when the request named it (a new feed's is in its zip)
    gtfs_id      text,
    kind         text        NOT NULL CHECK (kind IN ('seed', 'drafts')),
    dry_run      boolean     NOT NULL,
    status       text        NOT NULL DEFAULT 'running'
                             CHECK (status IN ('running', 'done', 'failed')),
    -- JSON: the import's report once done; {status, code, message} once failed
    report       jsonb,
    error        jsonb,
    created_by   uuid        REFERENCES gtfs_editor_user (user_id),
    -- the pod running it: a job still running after its pod went away is lost
    pod          text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz
);

CREATE INDEX IF NOT EXISTS gtfs_import_job_created_idx
    ON gtfs_import_job (created_at DESC);

COMMIT;
