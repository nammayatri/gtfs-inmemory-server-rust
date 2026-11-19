ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_window_check CHECK (window_end_at > window_start_at);
