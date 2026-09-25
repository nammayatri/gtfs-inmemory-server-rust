-- A stop headsign the feed owns, so a DB feed is not forced into MTC's fare
-- stages (docs/gtfs-editor.md section 1, "The headsign").
--
-- The loader used to synthesise every headsign from `stage_no` / `stop_type`:
-- the stage number, or `{'fareStageNumber': 'N', 'isStageStop': true}` on a
-- NEW STOP. That is what chennai_bus serves and must keep serving, but it is
-- MTC bus semantics: every other feed serves `headsign: null` today, so
-- migrating one to these tables as they stood would have turned its riders'
-- null into a stage number it has no stages for.
--
-- So a headsign now has two sources, in order:
--
--   gtfs_route_stop.stop_headsign  the row's own, GTFS stop_headsign. Nullable,
--                                  and null on every row that exists today.
--   gtfs_feed.headsign_source      what a row with none falls back to:
--                                  'fare_stage' synthesises from the stage,
--                                  'none' serves no headsign at all.
--
-- The backfill sets 'fare_stage' on every feed that exists when this runs -
-- chennai_bus on master, and whatever local test feeds a developer has - because
-- those are exactly the feeds the old code already synthesised for. A feed
-- seeded after this migration gets the 'none' default and serves what it serves
-- today, which is the whole point. It runs only when the column is created, so a
-- second run cannot undo a feed an admin has since moved to 'none'.
--
-- `stage_no` / `stage_name` stay NOT NULL - the editor and the MTC export read
-- them on every row - but gain defaults, because a feed with no fare stages has
-- nothing to put there. 0 / '' is that feed's "no stage", and with the headsign
-- decoupled nothing public reads either column.
--
-- The colour CHECKs are widened to accept GTFS's own spelling as well as the
-- `#rrggbb` one they were written for: `routes.txt` carries `route_color` as six
-- hex digits with no `#`, which is what every preprocessed feed serves and
-- therefore what a seeded feed has to store to serve the same bytes back.
-- chennai_bus has no colours at all, so nothing existing moves.
--
-- Safe to run twice.

BEGIN;

ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS stop_headsign text;

ALTER TABLE gtfs_route_stop ALTER COLUMN stage_no   SET DEFAULT 0;
ALTER TABLE gtfs_route_stop ALTER COLUMN stage_name SET DEFAULT '';

ALTER TABLE gtfs_route DROP CONSTRAINT IF EXISTS gtfs_route_color_check;
ALTER TABLE gtfs_route ADD  CONSTRAINT gtfs_route_color_check
    CHECK (color IS NULL OR color ~ '^#?[0-9A-Fa-f]{6}$');
ALTER TABLE gtfs_route DROP CONSTRAINT IF EXISTS gtfs_route_text_color_check;
ALTER TABLE gtfs_route ADD  CONSTRAINT gtfs_route_text_color_check
    CHECK (text_color IS NULL OR text_color ~ '^#?[0-9A-Fa-f]{6}$');

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM information_schema.columns
                    WHERE table_name = 'gtfs_feed' AND column_name = 'headsign_source') THEN
        ALTER TABLE gtfs_feed ADD COLUMN headsign_source text NOT NULL DEFAULT 'none'
            CHECK (headsign_source IN ('none', 'fare_stage'));
        UPDATE gtfs_feed SET headsign_source = 'fare_stage';
    END IF;
END $$;

COMMIT;
