CREATE INDEX idx_employees_designation ON public.employees_internal USING btree (gtfs_id, designation_id, first_name) WHERE (deleted = false);
