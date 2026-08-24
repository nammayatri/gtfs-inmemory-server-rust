ALTER TABLE ONLY public.bus_schedule_trip_detail
    ADD CONSTRAINT fkb7k7yiwxnfyon1bhdxj4s3ohc FOREIGN KEY (schedule_trip_id) REFERENCES public.bus_schedule_trip(schedule_trip_id);
