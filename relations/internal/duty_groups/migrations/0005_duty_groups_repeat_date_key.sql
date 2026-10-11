-- One run per repeat rule per day. Not partial: a cancelled / deleted day still blocks
-- regeneration. NULL duty_repeat_id (manual runs) never collides. Also the ON CONFLICT arbiter
-- for generation.
ALTER TABLE ONLY public.duty_groups
    ADD CONSTRAINT duty_groups_duty_repeat_id_operation_date_key UNIQUE (duty_repeat_id, operation_date);
