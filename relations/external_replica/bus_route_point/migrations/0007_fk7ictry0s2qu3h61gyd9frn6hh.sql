ALTER TABLE ONLY public.bus_route_point
    ADD CONSTRAINT fk7ictry0s2qu3h61gyd9frn6hh FOREIGN KEY (route_id) REFERENCES public.bus_route(route_id);
