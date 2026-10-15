CREATE INDEX idx_waybills_online_schedule_trip ON public.waybills USING btree (schedule_trip_id) INCLUDE (waybill_id, is_flexi) WHERE (((status)::text = 'Online'::text) AND (deleted = false));
