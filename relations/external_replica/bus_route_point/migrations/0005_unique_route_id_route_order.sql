ALTER TABLE ONLY public.bus_route_point
    ADD CONSTRAINT unique_route_id_route_order UNIQUE (route_id, route_order);
