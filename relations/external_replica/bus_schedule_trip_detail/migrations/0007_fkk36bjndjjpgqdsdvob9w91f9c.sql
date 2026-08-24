ALTER TABLE ONLY public.bus_schedule_trip_detail
    ADD CONSTRAINT fkk36bjndjjpgqdsdvob9w91f9c FOREIGN KEY (route_number_id) REFERENCES public.bus_route(route_id);
