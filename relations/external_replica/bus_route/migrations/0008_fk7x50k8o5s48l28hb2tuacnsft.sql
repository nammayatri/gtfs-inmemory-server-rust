ALTER TABLE ONLY public.bus_route
    ADD CONSTRAINT fk7x50k8o5s48l28hb2tuacnsft FOREIGN KEY (end_point_id) REFERENCES public.bus_stop(bus_stop_id);
