CREATE INDEX idx_duties_driver_all ON public.duties USING btree (gtfs_id, driver_token_number, duty_group_id) WHERE (NOT deleted);
