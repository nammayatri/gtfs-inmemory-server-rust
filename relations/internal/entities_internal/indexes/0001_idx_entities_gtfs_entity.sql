CREATE INDEX idx_entities_gtfs_entity ON public.entities_internal USING btree (gtfs_id, entity_id) WHERE (deleted = false);
