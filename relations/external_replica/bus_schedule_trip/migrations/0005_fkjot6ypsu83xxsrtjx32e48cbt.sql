ALTER TABLE ONLY public.bus_schedule_trip
    ADD CONSTRAINT fkjot6ypsu83xxsrtjx32e48cbt FOREIGN KEY (schedule_id) REFERENCES public.bus_schedule(schedule_id);
