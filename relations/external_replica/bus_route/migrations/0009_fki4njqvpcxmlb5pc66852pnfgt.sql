ALTER TABLE ONLY public.bus_route
    ADD CONSTRAINT fki4njqvpcxmlb5pc66852pnfgt FOREIGN KEY (bus_service_type_id) REFERENCES public.bus_service_type(service_type_id);
