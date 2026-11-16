CREATE INDEX idx_duties_driver_trgm ON public.duties USING gin (driver_token_number public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_duties_conductor_trgm ON public.duties USING gin (conductor_token_number public.gin_trgm_ops) WHERE (NOT deleted);
