CREATE INDEX idx_waybills_waybillno ON public.waybills_internal USING btree (gtfs_id, waybill_no) WHERE (deleted = false);
