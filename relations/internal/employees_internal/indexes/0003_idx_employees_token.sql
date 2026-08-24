CREATE INDEX idx_employees_token ON public.employees_internal USING btree (gtfs_id, token_no) WHERE (deleted = false);
