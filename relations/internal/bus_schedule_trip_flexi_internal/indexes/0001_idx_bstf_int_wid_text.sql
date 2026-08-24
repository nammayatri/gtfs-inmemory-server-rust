CREATE INDEX idx_bstf_int_wid_text ON public.bus_schedule_trip_flexi_internal USING btree (waybill_id, gtfs_id) WHERE (deleted = false);
