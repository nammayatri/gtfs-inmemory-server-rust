-- Admin self-approval (docs/gtfs-editor.md section 2): an admin may approve, and
-- then commit, a change set they submitted, by saying so explicitly. The set is
-- marked self_approved, and the maker-checker CHECK lets the submitter be the
-- reviewer only on a set so marked - never by accident, never for anyone whose
-- approval did not go through the override.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_change_set ADD COLUMN IF NOT EXISTS self_approved boolean NOT NULL DEFAULT false;

-- 0001 left the maker-checker CHECK unnamed: gtfs_change_set_check
ALTER TABLE gtfs_change_set DROP CONSTRAINT IF EXISTS gtfs_change_set_check;
ALTER TABLE gtfs_change_set DROP CONSTRAINT IF EXISTS gtfs_change_set_maker_checker_check;
ALTER TABLE gtfs_change_set ADD CONSTRAINT gtfs_change_set_maker_checker_check
    CHECK (reviewed_by IS NULL OR reviewed_by IS DISTINCT FROM submitted_by OR self_approved);

-- the mark is only ever on a set its submitter approved
ALTER TABLE gtfs_change_set DROP CONSTRAINT IF EXISTS gtfs_change_set_self_approved_check;
ALTER TABLE gtfs_change_set ADD CONSTRAINT gtfs_change_set_self_approved_check
    CHECK (NOT self_approved OR (reviewed_by IS NOT NULL AND reviewed_by = submitted_by));

COMMIT;
