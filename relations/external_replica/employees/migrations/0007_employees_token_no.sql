ALTER TABLE ONLY public.employees
    ADD CONSTRAINT employees_token_no UNIQUE (token_no);
