CREATE UNIQUE INDEX eta_variant_code_idx ON public.eta_variant USING btree (gtfs_id, code) WHERE (NOT deleted);
