-- Closing or reopening a stage review is not an edit of the stage
-- (docs/gtfs-editor.md section 19, "Stage reviews").
--
-- A review's close and reopen set gtfs_stage.review, and the shared
-- gtfs_touch_row trigger bumped row_version on every UPDATE. A draft's
-- stage/update made from the review carries the version it was made against,
-- so the documented order - fix the stage in a draft, then Mark fixed naming
-- that draft - left the draft conflicting with itself ("changed by another
-- commit after this edit was made") and it could not be submitted.
--
-- gtfs_stage gets a trigger of its own: an UPDATE that changes only the review
-- flag (and who/when) keeps row_version; any other change bumps it as before,
-- including the deliberate `SET updated_by` a stop merge uses to mark the
-- stages whose stops it moved.
--
-- Safe to run twice. Before it, a close still conflicts with a draft's edit of
-- the stage; nothing else changes.

BEGIN;

CREATE OR REPLACE FUNCTION gtfs_stage_touch_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    NEW.updated_at := now();
    IF NEW.review IS DISTINCT FROM OLD.review
       AND (to_jsonb(NEW) - 'review' - 'updated_at' - 'updated_by' - 'row_version')
         = (to_jsonb(OLD) - 'review' - 'updated_at' - 'updated_by' - 'row_version') THEN
        NEW.row_version := OLD.row_version;
    ELSE
        NEW.row_version := OLD.row_version + 1;
    END IF;
    RETURN NEW;
END $$;

DROP TRIGGER IF EXISTS gtfs_stage_touch ON gtfs_stage;
CREATE TRIGGER gtfs_stage_touch BEFORE UPDATE ON gtfs_stage
    FOR EACH ROW EXECUTE FUNCTION gtfs_stage_touch_row();

COMMIT;
