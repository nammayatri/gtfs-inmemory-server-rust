ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_depot_fk FOREIGN KEY (depot_id) REFERENCES public.entities_internal(entity_id);
