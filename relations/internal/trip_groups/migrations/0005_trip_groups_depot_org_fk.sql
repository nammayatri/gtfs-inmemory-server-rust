ALTER TABLE ONLY public.trip_groups
    ADD CONSTRAINT trip_groups_depot_org_fk FOREIGN KEY (depot_id, organization_id) REFERENCES public.entities_internal(entity_id, organization_id) ON UPDATE CASCADE;
