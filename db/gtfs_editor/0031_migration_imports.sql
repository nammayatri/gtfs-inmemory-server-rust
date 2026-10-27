-- Migrations from master read in on prod (docs/gtfs-editor.md section 23).
--
-- Master and prod each have their own editor database. A migration carries
-- master's work to prod as a file, and what it brings becomes ordinary prod
-- drafts. Each read-in that drafted something is recorded here: what the file
-- was, which drafts it made, and its report. A full migration reads this back -
-- the changes of these drafts are the migration's, every other committed change
-- is prod's own and is kept - and a draft brought once is not brought again.
--
-- Also: a migration read in the background is an import job like the others.
--
-- Additive; the image before it never reads it. Apply it before the image that
-- has section 23, on master and on prod. Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_migration_import (
    import_id      uuid        PRIMARY KEY,
    gtfs_id        text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    -- change_set: one committed master draft; full: the whole feed, a step at a time
    kind           text        NOT NULL CHECK (kind IN ('change_set', 'full')),
    -- full: the step this read-in drafted
    step           text,
    -- the file's manifest: where it came from, the master draft, what it carries
    source         jsonb       NOT NULL,
    file_sha256    text        NOT NULL,
    -- full: from which prod version on prod's own changes are kept
    since_version  bigint,
    change_set_ids uuid[]      NOT NULL DEFAULT '{}',
    report         jsonb       NOT NULL,
    imported_by    uuid        NOT NULL REFERENCES gtfs_editor_user (user_id),
    imported_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS gtfs_migration_import_feed_idx
    ON gtfs_migration_import (gtfs_id, imported_at DESC);
-- "is this draft a migration's?", asked of every committed draft a full
-- migration reads
CREATE INDEX IF NOT EXISTS gtfs_migration_import_sets_idx
    ON gtfs_migration_import USING gin (change_set_ids);

-- every kind 0024 and 0025 allow, and the new one
ALTER TABLE gtfs_import_job DROP CONSTRAINT IF EXISTS gtfs_import_job_kind_check;
ALTER TABLE gtfs_import_job ADD CONSTRAINT gtfs_import_job_kind_check
    CHECK (kind IN ('seed', 'drafts', 'gps_line', 'migration'));

COMMIT;
