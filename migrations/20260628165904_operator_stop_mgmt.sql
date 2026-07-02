ALTER TABLE stop_internal ADD COLUMN IF NOT EXISTS source text;

ALTER TABLE route_point_internal ADD COLUMN IF NOT EXISTS stop_type text;
ALTER TABLE route_point_internal ADD COLUMN IF NOT EXISTS stage_name text;
ALTER TABLE route_point_internal ADD COLUMN IF NOT EXISTS is_visible boolean DEFAULT true;

ALTER TABLE route_internal ADD COLUMN IF NOT EXISTS encoded_polyline text;

ALTER TABLE route_point_internal ALTER COLUMN travel_distance DROP NOT NULL;

CREATE INDEX CONCURRENTLY IF NOT EXISTS rp_route_order_idx
  ON route_point_internal (route_id, route_order) WHERE deleted = false;
CREATE INDEX CONCURRENTLY IF NOT EXISTS rp_bus_stop_idx
  ON route_point_internal (bus_stop_id) WHERE deleted = false;

CREATE EXTENSION IF NOT EXISTS pg_trgm;
CREATE INDEX CONCURRENTLY IF NOT EXISTS stop_name_trgm_idx
  ON stop_internal USING gin (bus_stop_name gin_trgm_ops);

CREATE INDEX CONCURRENTLY IF NOT EXISTS stop_latlon_idx
  ON stop_internal (latitude_current, longitude_current) WHERE deleted = false;

CREATE EXTENSION IF NOT EXISTS postgis;
ALTER TABLE stop_internal ADD COLUMN IF NOT EXISTS geom geometry(Point, 4326)
  GENERATED ALWAYS AS (ST_SetSRID(ST_MakePoint(longitude_current, latitude_current), 4326)) STORED;
CREATE INDEX CONCURRENTLY IF NOT EXISTS stop_geom_idx
  ON stop_internal USING GIST (geom) WHERE deleted = false;

ALTER TABLE route_internal ADD COLUMN IF NOT EXISTS encoded_polyline text;