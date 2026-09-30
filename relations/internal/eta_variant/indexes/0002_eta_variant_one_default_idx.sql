CREATE UNIQUE INDEX eta_variant_one_default_idx ON public.eta_variant USING btree (gtfs_id) WHERE (is_default AND (NOT deleted));
