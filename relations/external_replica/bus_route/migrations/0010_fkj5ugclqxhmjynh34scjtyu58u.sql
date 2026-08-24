ALTER TABLE ONLY public.bus_route
    ADD CONSTRAINT fkj5ugclqxhmjynh34scjtyu58u FOREIGN KEY (start_point_id) REFERENCES public.bus_stop(bus_stop_id);
