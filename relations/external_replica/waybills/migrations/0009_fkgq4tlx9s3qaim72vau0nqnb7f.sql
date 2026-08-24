ALTER TABLE ONLY public.waybills
    ADD CONSTRAINT fkgq4tlx9s3qaim72vau0nqnb7f FOREIGN KEY (entity_id) REFERENCES public.entities(entity_id);
