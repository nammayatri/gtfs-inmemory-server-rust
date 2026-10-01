-- A bus can't be on two active runs whose windows overlap. Deferred so swaps inside one
-- transaction work; violations surface at COMMIT as SQLSTATE 23P01.
ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT dg_vehicle_no_overlap EXCLUDE USING gist (
        gtfs_id WITH =, vehicle_number WITH =,
        tstzrange(window_start_at, window_end_at) WITH &&
    ) WHERE (is_active AND NOT deleted) DEFERRABLE INITIALLY DEFERRED;
