ALTER TABLE ONLY public.bus_schedule
    ADD CONSTRAINT fkpiel9bprpawy1f1ulxrh6h7m0 FOREIGN KEY (entity_id) REFERENCES public.entities(entity_id);
