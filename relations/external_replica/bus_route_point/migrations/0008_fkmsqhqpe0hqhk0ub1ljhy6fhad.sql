ALTER TABLE ONLY public.bus_route_point
    ADD CONSTRAINT fkmsqhqpe0hqhk0ub1ljhy6fhad FOREIGN KEY (bus_stop_id) REFERENCES public.bus_stop(bus_stop_id);
