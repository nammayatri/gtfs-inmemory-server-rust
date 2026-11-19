ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_duty_repeat_id_operation_date_key UNIQUE (duty_repeat_id, operation_date);
