-- A driver can't be on two overlapping trips (needs btree_gist, duty_groups/0001).
ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_driver_no_overlap EXCLUDE USING gist (
        gtfs_id WITH =, driver_token_number WITH =,
        tstzrange(scheduled_start_at, scheduled_end_at) WITH &&
    ) WHERE (run_active AND status <> 'cancelled' AND NOT deleted) DEFERRABLE INITIALLY DEFERRED;
