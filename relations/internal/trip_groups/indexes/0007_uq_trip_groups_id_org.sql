CREATE UNIQUE INDEX uq_trip_groups_id_org ON public.trip_groups USING btree (id, organization_id);
