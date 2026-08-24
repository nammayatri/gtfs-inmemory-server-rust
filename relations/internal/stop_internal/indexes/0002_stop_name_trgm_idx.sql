CREATE INDEX stop_name_trgm_idx ON public.stop_internal USING gin (bus_stop_name public.gin_trgm_ops);
