ALTER TABLE ONLY public.vehicles
    ADD CONSTRAINT fk5sk6v2v8ed6t59oh1ybh2vjww FOREIGN KEY (bus_service_type_id) REFERENCES public.bus_service_type(service_type_id);
