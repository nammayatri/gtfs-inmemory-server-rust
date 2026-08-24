ALTER TABLE ONLY public.waybills
    ADD CONSTRAINT unique_waybill_no_idx UNIQUE (waybill_no);
