-- The webhook policy - whether webhooks may fire at all, and which hosts they
-- may be pointed at - as a row the dashboard can edit, superseding the dhall
-- config (docs/gtfs-editor.md section 12.5).
--
-- Until now both values lived only in the deployment's configmap, so adding one
-- host meant an engineer editing a shared file and restarting every pod. That
-- is the wrong shape for a list that ops discover they need one entry at a time,
-- usually while a delivery is failing.
--
-- The precedence rule is the one this system already has for
-- gtfs_feed.data_source over the static gtfs_db_feeds list (0008): the row
-- wins, and the config value is only the seed used when there is no row. One
-- rule, not two - so "what is in effect" is answered the same way everywhere.
-- No row at all means the deployment's own values are in effect, which is what
-- every existing deployment gets until someone saves from the dashboard.
--
-- The table holds exactly one row, ever. The policy is a property of the
-- deployment, not of a feed, and a second row would raise the question of which
-- one is in force. The singleton PRIMARY KEY with its CHECK makes that
-- unrepresentable rather than merely discouraged.
--
-- Safe to run twice.

BEGIN;

CREATE TABLE IF NOT EXISTS gtfs_webhook_settings (
    -- always true: the PRIMARY KEY allows one row and the CHECK stops a second
    -- value from ever existing to have a second row under
    singleton     boolean     PRIMARY KEY DEFAULT true CHECK (singleton),
    enabled       boolean     NOT NULL DEFAULT false,
    -- hosts a webhook URL may point at, each a plain hostname, or one written
    -- `.example.com` to match that domain and its subdomains. Empty means no
    -- webhook can fire even when enabled: the feature fails closed, exactly as
    -- an empty gtfs_webhook_allowed_hosts does.
    allowed_hosts text[]      NOT NULL DEFAULT '{}',
    updated_at    timestamptz NOT NULL DEFAULT now(),
    updated_by    text
);

-- keep updated_at honest, like every other editor table
DROP TRIGGER IF EXISTS gtfs_webhook_settings_touch ON gtfs_webhook_settings;
CREATE TRIGGER gtfs_webhook_settings_touch
    BEFORE UPDATE ON gtfs_webhook_settings
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

COMMIT;
