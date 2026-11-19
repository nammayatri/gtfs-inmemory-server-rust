CREATE UNIQUE INDEX uq_duties_one_active_per_group ON public.duties USING btree (duty_group_id) WHERE (status = 'active' AND NOT deleted);
