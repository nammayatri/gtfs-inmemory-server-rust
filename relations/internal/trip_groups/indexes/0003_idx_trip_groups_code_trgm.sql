CREATE INDEX idx_trip_groups_code_trgm ON public.trip_groups USING gin (code public.gin_trgm_ops) WHERE (NOT deleted);
