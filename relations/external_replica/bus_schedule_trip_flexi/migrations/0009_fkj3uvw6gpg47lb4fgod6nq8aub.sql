ALTER TABLE ONLY public.bus_schedule_trip_flexi
    ADD CONSTRAINT fkj3uvw6gpg47lb4fgod6nq8aub FOREIGN KEY (schedule_trip_id) REFERENCES public.bus_schedule_trip(schedule_trip_id);
