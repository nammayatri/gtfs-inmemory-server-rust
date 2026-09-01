ALTER TABLE public.vehicles_internal
    ADD COLUMN IF NOT EXISTS tag_number text;
