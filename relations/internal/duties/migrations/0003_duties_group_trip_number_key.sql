ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_duty_group_id_trip_number_key UNIQUE (duty_group_id, trip_number);
