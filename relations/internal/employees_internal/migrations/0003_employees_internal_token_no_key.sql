ALTER TABLE ONLY public.employees_internal
    ADD CONSTRAINT employees_internal_token_no_key UNIQUE (token_no);
