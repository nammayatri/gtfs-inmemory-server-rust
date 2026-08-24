ALTER TABLE ONLY public.bus_schedule
    ADD CONSTRAINT fk2rt5vh0xn4ldhevke75rhe141 FOREIGN KEY (route_id) REFERENCES public.bus_route(route_id);
