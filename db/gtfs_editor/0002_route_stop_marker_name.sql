-- ROUTE CORRECTION rows carry a name in the mapping (VANAVIL, HIGH COURT, ...);
-- keep it so an export reproduces clean_route_mapping.csv row for row.
ALTER TABLE gtfs_route_stop ADD COLUMN marker_name text;
