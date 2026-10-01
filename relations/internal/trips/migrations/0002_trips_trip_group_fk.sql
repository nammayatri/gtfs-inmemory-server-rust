ALTER TABLE ONLY public.trips
    ADD CONSTRAINT trips_trip_group_id_fkey FOREIGN KEY (trip_group_id) REFERENCES public.trip_groups(id);
