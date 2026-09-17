-- Make gtfs_feed.data_source live: GIMS's poll loop now reads this column each
-- cycle to decide which feeds it serves from these tables instead of from a
-- pod's static `gtfs_db_feeds` config, so an operator can flip a feed between
-- 'db' and 'preprocessed' from the editor dashboard without a restart.
--
-- NOTE: the column and its CHECK ('db', 'preprocessed') already exist -
-- 0001_create_gtfs_editor.sql created gtfs_feed with data_source from the
-- start, as a display-only field the dashboard showed but nothing acted on.
-- There is nothing to ALTER here. This migration only backfills chennai_bus's
-- row: it is the one feed already served from these tables today (via the
-- static config), so its row should say 'db' now that the column is
-- authoritative, not the 'preprocessed' default it was left at when the
-- column was purely informational.
--
-- The UPDATE is a no-op (0 rows) on a fresh DB that has no chennai_bus row
-- yet, and bumps version like every other write path here (see
-- gtfs_change_set_commit's `UPDATE gtfs_feed SET version = version + 1`) so a
-- running GIMS pod's poll loop notices and picks up the backfill.

BEGIN;

UPDATE gtfs_feed
   SET data_source = 'db',
       version = version + 1
 WHERE gtfs_id = 'chennai_bus'
   AND data_source <> 'db';

COMMIT;
