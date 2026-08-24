CREATE INDEX idx_entities_internal_entity_gtfs_full ON public.entities_internal USING btree (entity_id, gtfs_id);
