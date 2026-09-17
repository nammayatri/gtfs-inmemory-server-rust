-- stop/merge: a duplicate stop's route rows move to the stop that stays, and the
-- duplicate is soft-deleted (docs/gtfs-editor.md section 5). A change's op may
-- now be 'merge'.

BEGIN;

ALTER TABLE gtfs_change DROP CONSTRAINT gtfs_change_op_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_op_check
    CHECK (op IN ('create', 'update', 'delete', 'replace', 'merge'));

COMMIT;
