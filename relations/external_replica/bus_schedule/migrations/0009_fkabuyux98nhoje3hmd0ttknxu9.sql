ALTER TABLE ONLY public.bus_schedule
    ADD CONSTRAINT fkabuyux98nhoje3hmd0ttknxu9 FOREIGN KEY (service_type_id) REFERENCES public.bus_service_type(service_type_id);
