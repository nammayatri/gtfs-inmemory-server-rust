ALTER TABLE ONLY public.trip_groups
    ADD CONSTRAINT trip_groups_depot_fk FOREIGN KEY (depot_id) REFERENCES public.entities_internal(entity_id);
