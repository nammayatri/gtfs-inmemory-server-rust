-- Indexes behind the cleanup context reads (docs/gtfs-editor.md section 9):
-- `GET /feeds/{g}/stops/{stop_id}/context` and `.../routes/{route_id}/context`
-- ask the audit log "what happened to this stop / route" and the review queue
-- "which reviews has this stop had", and neither table could answer by stop.
--
--   - audit rows that name a stop in detail.stop_id (every position_review_*
--     row does) or a change's row in detail.entity_key (change_added);
--   - stop_merged rows by either side of the merge;
--   - a stop's reviews in every status (the open-review unique index covers
--     only pending and approved).
--
-- The audit log is append-only; an index is not an update. Safe to run twice.

BEGIN;

CREATE INDEX IF NOT EXISTS gtfs_audit_log_stop_idx
    ON gtfs_audit_log (gtfs_id, (detail->>'stop_id'), audit_id DESC)
    WHERE detail ? 'stop_id';
CREATE INDEX IF NOT EXISTS gtfs_audit_log_entity_key_idx
    ON gtfs_audit_log (gtfs_id, (detail->>'entity_key'), audit_id DESC)
    WHERE detail ? 'entity_key';
CREATE INDEX IF NOT EXISTS gtfs_audit_log_merge_from_idx
    ON gtfs_audit_log (gtfs_id, (detail->>'from'), audit_id DESC)
    WHERE action = 'stop_merged';
CREATE INDEX IF NOT EXISTS gtfs_audit_log_merge_into_idx
    ON gtfs_audit_log (gtfs_id, (detail->>'into'), audit_id DESC)
    WHERE action = 'stop_merged';
CREATE INDEX IF NOT EXISTS gtfs_position_review_stop_idx
    ON gtfs_position_review (gtfs_id, stop_id, review_id);

COMMIT;
