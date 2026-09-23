-- The Release to Nandi button (docs/gtfs-editor.md section 12.6). A webhook on
-- `release_requested` is never fired by the dispatcher's own loop; it is sent
-- only when an approver presses the button, as a `kind = 'release'` delivery.
-- Like 'test', a 'release' delivery is outside the (webhook, version) unique
-- index, so it neither consumes nor satisfies an automatic delivery.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_webhook DROP CONSTRAINT IF EXISTS gtfs_webhook_event_check;
ALTER TABLE gtfs_webhook
    ADD CONSTRAINT gtfs_webhook_event_check
    CHECK (event IN ('feed_in_sync', 'feed_committed', 'feed_reload_failed', 'release_requested'));

ALTER TABLE gtfs_webhook_delivery DROP CONSTRAINT IF EXISTS gtfs_webhook_delivery_kind_check;
ALTER TABLE gtfs_webhook_delivery
    ADD CONSTRAINT gtfs_webhook_delivery_kind_check
    CHECK (kind IN ('event', 'test', 'release'));

-- which Nandi a release request is for: master and prod share this database,
-- so the pressing editor records it, whichever pod then sends the request
ALTER TABLE gtfs_webhook_delivery ADD COLUMN IF NOT EXISTS target text
    CHECK (target IN ('master', 'prod'));

-- the button's in-progress check reads the feed's recent release requests
CREATE INDEX IF NOT EXISTS gtfs_webhook_delivery_release_idx
    ON gtfs_webhook_delivery (gtfs_id, created_at DESC)
    WHERE kind = 'release';

COMMIT;
