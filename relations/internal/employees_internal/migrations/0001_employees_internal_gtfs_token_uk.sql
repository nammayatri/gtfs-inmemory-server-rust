ALTER TABLE ONLY public.employees_internal
    ADD CONSTRAINT employees_internal_gtfs_token_uk UNIQUE (gtfs_id, token_no);
