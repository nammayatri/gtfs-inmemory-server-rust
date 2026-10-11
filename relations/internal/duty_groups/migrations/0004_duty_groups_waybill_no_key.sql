ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_waybill_no_key UNIQUE (waybill_no);
