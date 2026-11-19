ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_trip_group_org_fk FOREIGN KEY (trip_group_id, organization_id) REFERENCES public.trip_groups(id, organization_id) ON UPDATE CASCADE;
