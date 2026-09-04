ALTER TABLE ONLY public.bus_schedule_trip_flexi
    ADD CONSTRAINT fk4bc1iiw09bf5hmqn4g04sbils FOREIGN KEY (waybill_id) REFERENCES public.waybills(waybill_id);
