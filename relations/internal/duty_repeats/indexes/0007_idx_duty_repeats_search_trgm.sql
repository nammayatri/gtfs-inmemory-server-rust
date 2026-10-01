CREATE INDEX idx_duty_repeats_vehicle_trgm ON public.duty_repeats USING gin (vehicle_number public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_duty_repeats_driver_trgm ON public.duty_repeats USING gin (driver_token_number public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_duty_repeats_conductor_trgm ON public.duty_repeats USING gin (conductor_token_number public.gin_trgm_ops) WHERE (NOT deleted);
