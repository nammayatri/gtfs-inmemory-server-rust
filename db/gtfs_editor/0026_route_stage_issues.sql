-- Routes the backfill could not line up with MTC's own route definition.
--
-- The backfill names a fare stage by MTC's `bus_stop_id` for its head, which it
-- gets by lining our fare stages up against the replica's, by position, when the
-- two lists are the same length (docs/stage-backfill.md). When they are not, it
-- pairs nothing for that route rather than guess: a guess slides the shorter
-- list against the longer one and hands every stage after the divergence the
-- wrong id, silently and permanently. The route's stages then fall back to being
-- named by their own names, which is where `nm_SAIDAPET` comes from, and the
-- same real fare stage ends up as two stages - one MTC-keyed, one name-keyed.
--
-- Nothing is missing from a table: both sides are complete and they disagree.
-- Only a person who knows the route can say which side is right, so the routes
-- are marked here with both lists, and worked through like the stage reviews.
CREATE TABLE IF NOT EXISTS gtfs_route_stage_issue (
    issue_id      bigserial PRIMARY KEY,
    gtfs_id       text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    -- which run raised it, so a later run can supersede a whole batch
    batch         text        NOT NULL,
    route_id      text COLLATE "C" NOT NULL,
    -- what the route is called, kept here so the queue reads without a join
    short_name    text,
    -- count_differs  our fare stages and MTC's are a different number, so none
    --                of this route's stages could be given an MTC id
    -- order_differs  the same number, and they pair, but the names do not agree
    --                down the list, so the pairing may be wrong
    -- absent         MTC's replica does not carry this route at all
    issue         text        NOT NULL
                  CHECK (issue IN ('count_differs', 'order_differs', 'absent')),
    -- our fare stages in order: [{"stage_no": 1, "name": "BROADWAY"}, ...]
    ours          jsonb       NOT NULL DEFAULT '[]'::jsonb,
    -- MTC's, in order: [{"name": "ROYAPURAM B.T", "bus_stop_id": "148"}, ...]
    theirs        jsonb       NOT NULL DEFAULT '[]'::jsonb,
    -- how many of the route's stages carry a name-keyed id because of this: the
    -- weight of the row, and what the queue is worked in
    stages_unkeyed integer    NOT NULL DEFAULT 0,
    -- pending    nobody has looked
    -- fixed      looked at and put right, usually in the draft named below
    -- confirmed  looked at, nothing to change: MTC is ahead of us, or we of MTC
    -- superseded a later batch raised a fresh row for this route
    status        text        NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'fixed', 'confirmed', 'superseded')),
    change_set_id uuid        REFERENCES gtfs_change_set (change_set_id) ON DELETE SET NULL,
    reviewed_by   uuid        REFERENCES gtfs_editor_user (user_id),
    reviewed_at   timestamptz,
    review_note   text,
    created_at    timestamptz NOT NULL DEFAULT now(),
    updated_at    timestamptz NOT NULL DEFAULT now()
);
-- one open row per route; a closed one does not block the next
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_route_stage_issue_open_key
    ON gtfs_route_stage_issue (gtfs_id, route_id)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS gtfs_route_stage_issue_list_idx
    ON gtfs_route_stage_issue (gtfs_id, status, stages_unkeyed DESC, route_id);
CREATE INDEX IF NOT EXISTS gtfs_route_stage_issue_set_idx
    ON gtfs_route_stage_issue (change_set_id) WHERE change_set_id IS NOT NULL;

-- `stage/split` (docs section 19.1): one change makes a stage and moves the
-- routes named in it off another one, so a reviewer splitting a 47-route stage
-- writes one row here rather than 95 requests.
ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_op_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_op_check
    CHECK (op IN ('create', 'update', 'delete', 'replace', 'merge', 'activate', 'split'));
