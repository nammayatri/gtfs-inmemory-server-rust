ALTER TABLE ONLY public.waybills
    ADD CONSTRAINT fks8x4kp6g0bu879s9k9vj5esiq FOREIGN KEY (schedule_trip_id) REFERENCES public.bus_schedule_trip(schedule_trip_id);
