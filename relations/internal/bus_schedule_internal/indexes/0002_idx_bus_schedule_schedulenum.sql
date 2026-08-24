CREATE INDEX idx_bus_schedule_schedulenum ON public.bus_schedule_internal USING btree (schedule_number) WHERE (deleted = false);
