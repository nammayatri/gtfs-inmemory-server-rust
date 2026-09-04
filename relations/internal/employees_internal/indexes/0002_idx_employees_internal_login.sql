CREATE INDEX idx_employees_internal_login ON public.employees_internal USING btree (email_hash, password_hash, gtfs_id) WHERE (deleted = false);
