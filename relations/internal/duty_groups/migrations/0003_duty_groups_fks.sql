ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_trip_group_id_fkey FOREIGN KEY (trip_group_id) REFERENCES public.trip_groups(id);
ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_duty_repeat_id_fkey FOREIGN KEY (duty_repeat_id) REFERENCES public.duty_repeats(id);
