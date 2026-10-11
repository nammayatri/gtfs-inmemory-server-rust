ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_conductor_no_overlap EXCLUDE USING gist (
        gtfs_id WITH =, conductor_token_number WITH =,
        tstzrange(scheduled_start_at, scheduled_end_at) WITH &&
    ) WHERE (run_active AND status <> 'cancelled' AND NOT deleted) DEFERRABLE INITIALLY DEFERRED;
