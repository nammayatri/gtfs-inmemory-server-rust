ALTER TABLE ONLY public.waybill_device
    ADD CONSTRAINT fk6idmu2kisl5ooctlraorv7dmc FOREIGN KEY (waybill_id) REFERENCES public.waybills(waybill_id);
