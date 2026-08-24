CREATE INDEX idx_bstf_wid_text ON public.bus_schedule_trip_flexi USING btree (((waybill_id)::text)) WHERE (deleted = false);
