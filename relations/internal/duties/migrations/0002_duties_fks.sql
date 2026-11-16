ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_duty_group_id_fkey FOREIGN KEY (duty_group_id) REFERENCES public.duty_groups(id);
ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_trip_id_fkey FOREIGN KEY (trip_id) REFERENCES public.trips(id);
