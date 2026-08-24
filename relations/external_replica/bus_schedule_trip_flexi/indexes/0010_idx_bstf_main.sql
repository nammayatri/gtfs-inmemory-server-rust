CREATE INDEX idx_bstf_main ON public.bus_schedule_trip_flexi USING btree (waybill_id, trip_number);
