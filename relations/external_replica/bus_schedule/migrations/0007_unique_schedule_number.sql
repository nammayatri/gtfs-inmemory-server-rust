ALTER TABLE ONLY public.bus_schedule
    ADD CONSTRAINT unique_schedule_number UNIQUE (schedule_number);
