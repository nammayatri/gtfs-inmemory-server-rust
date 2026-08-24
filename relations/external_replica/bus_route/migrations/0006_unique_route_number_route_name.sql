ALTER TABLE ONLY public.bus_route
    ADD CONSTRAINT unique_route_number_route_name UNIQUE (route_number, route_name);
