ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_times_check CHECK (scheduled_end_at > scheduled_start_at);
