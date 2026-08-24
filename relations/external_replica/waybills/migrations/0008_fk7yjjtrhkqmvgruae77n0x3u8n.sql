ALTER TABLE ONLY public.waybills
    ADD CONSTRAINT fk7yjjtrhkqmvgruae77n0x3u8n FOREIGN KEY (service_type_id) REFERENCES public.bus_service_type(service_type_id);
