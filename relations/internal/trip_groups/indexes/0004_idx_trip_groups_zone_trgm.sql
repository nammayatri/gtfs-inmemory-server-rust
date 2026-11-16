CREATE INDEX idx_trip_groups_zone_trgm ON public.trip_groups USING gin (zone public.gin_trgm_ops) WHERE (NOT deleted);
