ALTER TABLE ONLY public.bus_route_point
    ADD CONSTRAINT unique_route_id_stage_no UNIQUE (route_id, stage_no);
