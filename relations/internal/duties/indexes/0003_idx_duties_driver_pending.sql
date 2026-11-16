CREATE INDEX idx_duties_driver_pending ON public.duties USING btree (gtfs_id, driver_token_number, scheduled_start_at) WHERE (status IN ('upcoming', 'active') AND NOT deleted AND run_active);
