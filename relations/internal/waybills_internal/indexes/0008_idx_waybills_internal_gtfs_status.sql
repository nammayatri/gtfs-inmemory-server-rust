CREATE INDEX idx_waybills_internal_gtfs_status ON public.waybills_internal USING btree (gtfs_id, status) WHERE (deleted = false);
