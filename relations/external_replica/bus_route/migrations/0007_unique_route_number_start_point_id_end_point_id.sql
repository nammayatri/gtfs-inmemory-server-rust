ALTER TABLE ONLY public.bus_route
    ADD CONSTRAINT unique_route_number_start_point_id_end_point_id UNIQUE (route_number, start_point_id, end_point_id);
