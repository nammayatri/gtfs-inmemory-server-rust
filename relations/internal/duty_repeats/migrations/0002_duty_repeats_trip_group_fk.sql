ALTER TABLE ONLY public.duty_repeats
    ADD CONSTRAINT duty_repeats_trip_group_id_fkey FOREIGN KEY (trip_group_id) REFERENCES public.trip_groups(id);
