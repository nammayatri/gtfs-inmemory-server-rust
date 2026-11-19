ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT dg_vehicle_no_overlap EXCLUDE USING gist (
        gtfs_id WITH =, vehicle_number WITH =,
        tstzrange(window_start_at, window_end_at) WITH &&
    ) WHERE (is_active AND NOT deleted) DEFERRABLE INITIALLY DEFERRED;
