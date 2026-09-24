-- Outbound webhooks, and the per-pod cache state one of their events is built
-- on (docs/gtfs-editor.md section 12).
--
-- A webhook is a URL GIMS calls when something happens to a feed. It is general
-- plumbing: a row picks one `event`, and the dispatcher delivers it exactly once
-- with retries. Adding an event later is a new value in the CHECK, not a new
-- table.
--
-- The event that motivated this is `feed_in_sync`. The frontline layer is static
-- files on S3 behind CloudFront, rebuilt and invalidated by a Jenkins job, and
-- that job must run *after* an edit is live on every pod - not when it was
-- committed. Fire on commit and a pod still holding the previous version can
-- answer a request that the freshly invalidated CloudFront passes through, and
-- CloudFront caches the stale answer again.
--
-- "Live on every pod" is something only the pods know, so each one says so here:
-- gtfs_pod_feed_state is a heartbeat carrying the feed version that pod has
-- loaded. The process that owns the cache is the one that reports it, so
-- nothing has to observe a pod from outside and guess.
--
-- Safe to run twice.

BEGIN;

-- ------------------------------------------------------------ pod heartbeats
-- One row per (feed, pod), rewritten every poll tick by the pod itself. A pod
-- that dies simply stops updating: staleness is read from updated_at, so there
-- is nothing to clean up on a crash, and a row left by a pod that will never
-- come back ages out of the fleet on its own.
--
-- boot_id distinguishes two lives of the same pod name (a restarting
-- StatefulSet pod keeps its name), so a restarted pod that has not finished
-- loading is never mistaken for the old one that had.
CREATE TABLE IF NOT EXISTS gtfs_pod_feed_state (
    gtfs_id        text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    pod_id         text COLLATE "C" NOT NULL,
    boot_id        uuid        NOT NULL,
    -- the gtfs_feed.version this pod is currently serving for this feed
    loaded_version bigint      NOT NULL,
    -- when loaded_version last *changed*, as against updated_at, which every
    -- heartbeat moves. The settle window is measured from the last pod to
    -- arrive at a version, so it needs the arrival time, not the last ping.
    loaded_at      timestamptz NOT NULL DEFAULT now(),
    -- 'db' while the pod serves the feed from these tables; 'preprocessed'
    -- after a revert, which keeps the row (the pod is still visibly alive) but
    -- takes it out of the fleet - it has no feed version to be in sync with.
    data_source    text        NOT NULL DEFAULT 'db'
                   CHECK (data_source IN ('db', 'preprocessed')),
    -- set when this pod's last attempt to load the feed failed, cleared by the
    -- next success. A pod that cannot load keeps serving what it had, so this
    -- is the only place that divergence is visible.
    last_error     text,
    failing_version bigint,
    image_tag      text,
    started_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (gtfs_id, pod_id)
);

-- the fleet check reads "every live pod of this feed", newest heartbeat first
CREATE INDEX IF NOT EXISTS gtfs_pod_feed_state_live_idx
    ON gtfs_pod_feed_state (gtfs_id, updated_at DESC);

-- ------------------------------------------------------------ webhooks
-- What to call, on what event, and when the fleet counts as settled. One feed
-- may have several; each is delivered independently.
--
-- url and the values in headers may contain ${ENV_VAR} placeholders, resolved
-- from the firing pod's own environment at the moment of the request. A
-- credential therefore stays in the Kubernetes secret the pod already mounts:
-- it is never written here, never returned by the API, and never reaches the
-- audit log or a delivery row. An unresolved placeholder fails the delivery
-- rather than sending the literal text.
CREATE TABLE IF NOT EXISTS gtfs_webhook (
    webhook_id     uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    gtfs_id        text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    name           text        NOT NULL,
    -- feed_in_sync      - every live pod serves the committed version (the
    --                     safe moment to rebuild a downstream cache)
    -- feed_committed    - a draft was committed; fires immediately, before the
    --                     pods have reloaded
    -- feed_reload_failed- a pod could not load a version and is serving older
    --                     data; an alert, not a trigger
    event          text        NOT NULL DEFAULT 'feed_in_sync'
                   CHECK (event IN ('feed_in_sync', 'feed_committed', 'feed_reload_failed')),
    url            text        NOT NULL,
    method         text        NOT NULL DEFAULT 'POST'
                   CHECK (method IN ('POST', 'PUT', 'GET')),
    headers        jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- request body. NULL sends the built-in JSON payload (feed, event, version,
    -- pods); an object is sent as JSON with ${...} resolved in its string
    -- leaves, which is how a Jenkins job's own parameter names are passed.
    body           jsonb,
    enabled        boolean     NOT NULL DEFAULT true,
    -- a pod whose heartbeat is older than this is treated as gone, not as a
    -- laggard: without it one dead pod would hold the fleet "out of sync"
    -- forever and nothing would ever fire
    stale_after_seconds  integer NOT NULL DEFAULT 60
                         CHECK (stale_after_seconds BETWEEN 10 AND 3600),
    -- how long the fleet must have been settled before firing. Absorbs a pod
    -- mid-rollout: it is not heartbeating yet, so it cannot be counted, and
    -- waiting makes it likely to have appeared by the time we fire.
    settle_seconds       integer NOT NULL DEFAULT 30
                         CHECK (settle_seconds BETWEEN 0 AND 3600),
    -- stop waiting for a version after this long, and record why. Without it a
    -- single unhealthy pod would suppress every future delivery in silence.
    give_up_after_seconds integer NOT NULL DEFAULT 1800
                          CHECK (give_up_after_seconds BETWEEN 60 AND 86400),
    request_timeout_seconds integer NOT NULL DEFAULT 30
                            CHECK (request_timeout_seconds BETWEEN 1 AND 300),
    max_attempts   integer     NOT NULL DEFAULT 5 CHECK (max_attempts BETWEEN 1 AND 20),
    created_at     timestamptz NOT NULL DEFAULT now(),
    created_by     text,
    updated_at     timestamptz NOT NULL DEFAULT now(),
    updated_by     text,
    UNIQUE (gtfs_id, name)
);

CREATE INDEX IF NOT EXISTS gtfs_webhook_feed_idx
    ON gtfs_webhook (gtfs_id, event) WHERE enabled;

-- ------------------------------------------------------------ deliveries
-- One row per (webhook, feed version). The unique index below is the whole
-- exactly-once mechanism: every pod notices the same moment and every one of
-- them tries to insert, but Postgres lets exactly one through, and only that
-- pod sends the request.
--
-- kind 'event' is the automatic one; 'test' is an admin pressing Test, which is
-- recorded the same way but does not consume the version's delivery.
CREATE TABLE IF NOT EXISTS gtfs_webhook_delivery (
    delivery_id    uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    webhook_id     uuid        NOT NULL REFERENCES gtfs_webhook (webhook_id) ON DELETE CASCADE,
    gtfs_id        text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    event          text        NOT NULL,
    feed_version   bigint      NOT NULL,
    kind           text        NOT NULL DEFAULT 'event'
                   CHECK (kind IN ('event', 'test')),
    status         text        NOT NULL DEFAULT 'pending'
                   CHECK (status IN ('pending', 'in_flight', 'succeeded', 'failed', 'abandoned')),
    attempts       integer     NOT NULL DEFAULT 0,
    -- when a retry becomes due; NULL once the delivery is finished
    next_attempt_at timestamptz,
    -- the fleet as it was when the delivery was created, for the audit trail
    pod_count      integer,
    pods           jsonb,
    response_status integer,
    last_error     text,
    -- the pod holding the delivery, and since when: a claim older than the
    -- request timeout is reclaimable, so a pod killed mid-request does not
    -- strand it
    claimed_by     text,
    claimed_at     timestamptz,
    requested_by   text,
    created_at     timestamptz NOT NULL DEFAULT now(),
    completed_at   timestamptz
);

-- the exactly-once key for automatic deliveries: one per (webhook, version).
-- Partial, because a 'test' delivery may repeat for the same version.
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_webhook_delivery_version_uk
    ON gtfs_webhook_delivery (webhook_id, feed_version)
    WHERE kind = 'event';

-- the dispatcher's own query: what is due now
CREATE INDEX IF NOT EXISTS gtfs_webhook_delivery_due_idx
    ON gtfs_webhook_delivery (next_attempt_at)
    WHERE status IN ('pending', 'in_flight');

CREATE INDEX IF NOT EXISTS gtfs_webhook_delivery_feed_idx
    ON gtfs_webhook_delivery (gtfs_id, created_at DESC);

-- keep updated_at honest on the config table, like every other editor table
DROP TRIGGER IF EXISTS gtfs_webhook_touch ON gtfs_webhook;
CREATE TRIGGER gtfs_webhook_touch
    BEFORE UPDATE ON gtfs_webhook
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

COMMIT;
