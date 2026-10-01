ALTER TABLE ONLY public.duty_repeats
    ADD CONSTRAINT duty_repeats_window_check CHECK (effective_till IS NULL OR effective_till >= effective_from);
