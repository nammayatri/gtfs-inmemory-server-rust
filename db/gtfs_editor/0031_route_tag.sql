-- A route's fare classification (docs/gtfs-editor.md section 18.18). Mirrors
-- `route_internal.route_tag`, the column nammayatri's FRFS stage-fare lookup
-- already reads to pick a route's fare row instead of the city's catch-all
-- (null routeTag). A route built in the tables -- a metro feeder, say -- has
-- no `route_internal` row to carry it, so the editor needs its own column and
-- its own way to set it.
--
-- Additive: an older image never reads the column, and GIMS reads it so that
-- a route with no tag answers exactly as it always has (`routeTag` omitted,
-- not null -- see NandiRoutesRes's skip_serializing_if). Safe to run twice.

BEGIN;

ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS route_tag text;

COMMIT;
