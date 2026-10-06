CREATE INDEX idx_dg_waybill_trgm ON public.duty_groups USING gin (waybill_no public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_dg_vehicle_trgm ON public.duty_groups USING gin (vehicle_number public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_dg_driver_trgm ON public.duty_groups USING gin (driver_token_number public.gin_trgm_ops) WHERE (NOT deleted);
CREATE INDEX idx_dg_conductor_trgm ON public.duty_groups USING gin (conductor_token_number public.gin_trgm_ops) WHERE (NOT deleted);
