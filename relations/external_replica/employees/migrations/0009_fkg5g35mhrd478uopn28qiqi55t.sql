ALTER TABLE ONLY public.employees
    ADD CONSTRAINT fkg5g35mhrd478uopn28qiqi55t FOREIGN KEY (entity_id) REFERENCES public.entities(entity_id);
