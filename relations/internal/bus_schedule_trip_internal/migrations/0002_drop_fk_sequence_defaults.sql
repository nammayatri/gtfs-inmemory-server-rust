-- These columns are foreign keys, not primary keys, but each was created with its own
-- `nextval(...)` default and NOT NULL — an artifact of the int->text id migration, where
-- every id column got a sequence whether or not it identified the row. The effect is that a
-- row inserted without an explicit parent id silently gets a fabricated id that matches no
-- parent, instead of NULL. Dropping the default and the NOT NULL makes "no parent set"
-- representable, so a dangling reference is visible as NULL rather than as a plausible id.
--
-- No table rewrite and no existing row is modified: DROP DEFAULT and DROP NOT NULL are
-- catalog-only. Existing fabricated ids stay as they are; this only changes what future
-- inserts that omit the column write.
--
-- The sequences themselves are left in place; dropping them is irreversible and is better
-- done separately once nothing is confirmed to reference them.

ALTER TABLE public.bus_schedule_trip_internal
    ALTER COLUMN calendar_id DROP DEFAULT,
    ALTER COLUMN calendar_id DROP NOT NULL,
    ALTER COLUMN schedule_id DROP DEFAULT,
    ALTER COLUMN schedule_id DROP NOT NULL;
