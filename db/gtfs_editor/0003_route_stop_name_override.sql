-- The mapping spells a few stop ids differently on some routes (TONDIARPET-1 vs
-- -2 DEPOT, CHENNAI vs TIRUSULAM AIRPORT, JUMP spellings). A stop has one name;
-- the row keeps the route's own spelling only where it differs, so an export
-- still reproduces clean_route_mapping.csv row for row.
ALTER TABLE gtfs_route_stop ADD COLUMN stop_name_override text;
ALTER TABLE gtfs_route_stop ADD CONSTRAINT gtfs_route_stop_override_only_on_stops
    CHECK (stop_name_override IS NULL OR stop_id IS NOT NULL);
