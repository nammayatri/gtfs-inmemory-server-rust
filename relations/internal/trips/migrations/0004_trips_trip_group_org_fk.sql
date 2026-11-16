ALTER TABLE ONLY public.trips
    ADD CONSTRAINT trips_trip_group_org_fk FOREIGN KEY (trip_group_id, organization_id) REFERENCES public.trip_groups(id, organization_id) ON UPDATE CASCADE;
