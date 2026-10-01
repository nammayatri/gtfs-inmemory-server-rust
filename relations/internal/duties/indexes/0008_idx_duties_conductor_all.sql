CREATE INDEX idx_duties_conductor_all ON public.duties USING btree (gtfs_id, conductor_token_number, duty_group_id) WHERE (NOT deleted);
