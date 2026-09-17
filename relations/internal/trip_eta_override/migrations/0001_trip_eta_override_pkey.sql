-- Keyed on the waybill rather than the schedule trip: one waybill is one vehicle, so four
-- buses running the same schedule are four independent rows. Overriding on the schedule row
-- would move all of them at once.

ALTER TABLE ONLY public.trip_eta_override
    ADD CONSTRAINT trip_eta_override_pkey PRIMARY KEY (gtfs_id, waybill_id, trip_number);
