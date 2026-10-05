-- Map lines from GPS as background jobs (docs/gtfs-editor.md section 17.10).
-- Reading a week of a busy route's pings can take minutes when the cluster is
-- slow, far past the 30 s the load balancer waits, so the dashboard asks for
-- the line in the background: the request answers at once with a job in
-- gtfs_import_job, of the new kind 'gps_line', and the page polls it.
--
-- Additive: the image before it never writes the new kind. Apply it before the
-- image that does. Safe to run twice.

BEGIN;

ALTER TABLE gtfs_import_job DROP CONSTRAINT IF EXISTS gtfs_import_job_kind_check;
ALTER TABLE gtfs_import_job ADD CONSTRAINT gtfs_import_job_kind_check
    CHECK (kind IN ('seed', 'drafts', 'gps_line'));

COMMIT;
