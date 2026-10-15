CREATE INDEX idx_employees_internal_mobile_no ON public.employees_internal USING btree (mobile_no) WHERE (deleted = false);
