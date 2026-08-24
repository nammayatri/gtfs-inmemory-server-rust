CREATE INDEX idx_waybills_online_live ON public.waybills USING btree (updated_at DESC) INCLUDE (schedule_trip_id, entity_id, is_flexi) WHERE (((status)::text = 'Online'::text) AND (deleted = false));
