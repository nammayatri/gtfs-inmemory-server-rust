CREATE UNIQUE INDEX uq_duty_groups_id_org ON public.duty_groups USING btree (id, organization_id);
