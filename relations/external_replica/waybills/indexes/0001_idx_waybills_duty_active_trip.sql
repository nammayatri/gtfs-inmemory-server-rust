CREATE INDEX idx_waybills_duty_active_trip ON public.waybills USING btree (duty_date, schedule_trip_id) WHERE (deleted = false);
