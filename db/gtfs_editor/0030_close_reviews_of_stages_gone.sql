-- Close the stage reviews whose stage a committed draft already took away
-- (docs/gtfs-editor.md section 19.1, "A stage merged away closes its review").
--
-- Merging a stage into another, or deleting it, left the stage's own review
-- pending: the stage was gone, but "Stages to review" still listed it, and a
-- search turned up stages that were long since dealt with. A commit now closes
-- such a review itself (stage_reviews::close_settled). This does the same once
-- for the drafts committed before that: each open review with no live stage of
-- its key left, whose stage a committed draft merged away or deleted after the
-- review was raised, is closed as fixed naming that draft and its author, and
-- the audit log says so.
--
-- A review naming its stage by the folded name (an older row) closes only once
-- every stage of that name and direction is gone. A review whose stage went
-- some other way (no committed draft took it) is left open for a person.
--
-- Safe to run twice: the second run finds nothing pending to close.

BEGIN;

WITH gone AS (
    SELECT DISTINCT ON (s.gtfs_id, s.stage_id, s.direction)
           s.gtfs_id, s.stage_id, s.direction, ch.op,
           split_part(ch.after->>'into_stage_id', '|', 1) AS into_id,
           upper(btrim(regexp_replace(s.name, '\s+', ' ', 'g'))) AS folded,
           cs.change_set_id, cs.title, cs.created_by, cs.committed_at
      FROM gtfs_change ch
      JOIN gtfs_change_set cs ON cs.change_set_id = ch.change_set_id
                             AND cs.status = 'committed'
      JOIN gtfs_stage s ON s.gtfs_id = cs.gtfs_id AND s.deleted
                       AND s.stage_id = split_part(ch.entity_key, '|', 1)
                       AND s.direction = split_part(ch.entity_key, '|', 2)
     WHERE ch.entity = 'stage' AND ch.op IN ('merge', 'delete')
     ORDER BY s.gtfs_id, s.stage_id, s.direction, cs.committed_at DESC
), closed AS (
    UPDATE gtfs_stage_review r
       SET status = 'fixed', reviewed_by = g.created_by, reviewed_at = g.committed_at,
           change_set_id = g.change_set_id,
           review_note = CASE WHEN g.op = 'merge'
               THEN format('Closed on its own: draft “%s” merged stage %s into %s.',
                           g.title, g.stage_id, g.into_id)
               ELSE format('Closed on its own: draft “%s” deleted stage %s.',
                           g.title, g.stage_id) END
      FROM gone g
     WHERE r.gtfs_id = g.gtfs_id AND r.status = 'pending' AND r.direction = g.direction
       AND (r.name_key = g.stage_id OR r.name_key = g.folded)
       AND r.created_at < g.committed_at
       AND NOT EXISTS (SELECT 1 FROM gtfs_stage s
            WHERE s.gtfs_id = r.gtfs_id AND NOT s.deleted AND s.direction = r.direction
              AND (s.stage_id = r.name_key
                   OR upper(btrim(regexp_replace(s.name, '\s+', ' ', 'g'))) = r.name_key))
    RETURNING r.review_id, r.gtfs_id, r.name, r.direction, r.reason,
              g.op, g.stage_id, g.into_id, g.change_set_id
)
INSERT INTO gtfs_audit_log (actor, actor_email, action, gtfs_id, change_set_id, detail)
SELECT NULL, 'migration 0030', 'stage_review_closed', c.gtfs_id, c.change_set_id,
       jsonb_build_object(
           'review_id', c.review_id, 'name', c.name,
           'direction', nullif(c.direction, ''), 'reason', c.reason,
           'decision', 'fixed', 'automatic', true, 'stage', c.stage_id,
           'merged_into', CASE WHEN c.op = 'merge' THEN c.into_id END)
  FROM closed c;

COMMIT;
