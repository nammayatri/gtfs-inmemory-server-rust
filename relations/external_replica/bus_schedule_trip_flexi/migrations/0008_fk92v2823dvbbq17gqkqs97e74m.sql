ALTER TABLE ONLY public.bus_schedule_trip_flexi
    ADD CONSTRAINT fk92v2823dvbbq17gqkqs97e74m FOREIGN KEY (route_number_id) REFERENCES public.bus_route(route_id);
