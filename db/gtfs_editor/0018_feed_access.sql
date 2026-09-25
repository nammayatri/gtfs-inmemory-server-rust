-- Feed access (docs/gtfs-editor.md section 15): who may work on which feed.
--
-- Until now a user's role applied to every feed. From here an admin is the only
-- global role; everyone else works on a feed only through a grant on that feed,
-- and the grant carries the role they hold there - viewer < editor < approver,
-- ordered as before. No grant: the feed does not exist for them. Only admins
-- grant and revoke (the editor API, audited as feed_access_granted / _changed /
-- _revoked), and a grant takes effect on the holder's next request: the editor
-- loads a user's grants with the user on every request.
--
-- gtfs_editor_user.role keeps its column and its values. From this migration
-- only 'admin' means anything; 'viewer' / 'editor' / 'approver' on a non-admin
-- stay as they were so that an older image rolled back onto this database still
-- works. This image ignores them.
--
-- kind = 'system' marks an account no person signs in with (the MTC sync of
-- section 16.8, created by its own migration). Every sign-in path refuses it
-- with 403 account_disabled, whatever SSO identity turns up with its email.
--
-- The backfill gives every non-admin a grant on chennai_bus, when that feed row
-- exists, at the role they hold today: it is the only feed anyone has edited,
-- so nobody loses anything and nobody gains a feed. It runs only when the table
-- is created, so a second run cannot hand chennai_bus back to someone an admin
-- has since revoked.
--
-- A grant has no ON DELETE on its feed: a feed row is never deleted on master,
-- and a script that deletes one (the flow tests' own feeds) says what happens
-- to its grants first.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_editor_user ADD COLUMN IF NOT EXISTS kind text NOT NULL DEFAULT 'person'
    CHECK (kind IN ('person', 'system'));

DO $$
BEGIN
    IF to_regclass('gtfs_editor_feed_access') IS NULL THEN
        CREATE TABLE gtfs_editor_feed_access (
            user_id     uuid        NOT NULL REFERENCES gtfs_editor_user (user_id) ON DELETE CASCADE,
            gtfs_id     text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
            role        text        NOT NULL CHECK (role IN ('viewer', 'editor', 'approver')),
            granted_by  uuid        REFERENCES gtfs_editor_user (user_id),
            granted_at  timestamptz NOT NULL DEFAULT now(),
            PRIMARY KEY (user_id, gtfs_id)
        );
        CREATE INDEX gtfs_editor_feed_access_feed_idx ON gtfs_editor_feed_access (gtfs_id);

        INSERT INTO gtfs_editor_feed_access (user_id, gtfs_id, role)
        SELECT u.user_id, f.gtfs_id, u.role
          FROM gtfs_editor_user u
          JOIN gtfs_feed f ON f.gtfs_id = 'chennai_bus'
         WHERE u.role <> 'admin'
        ON CONFLICT DO NOTHING;
    END IF;
END $$;

COMMIT;
