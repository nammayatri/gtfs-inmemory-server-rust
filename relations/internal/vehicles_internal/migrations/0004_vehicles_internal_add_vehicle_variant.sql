ALTER TABLE public.vehicles_internal
    ADD COLUMN IF NOT EXISTS vehicle_variant text;
