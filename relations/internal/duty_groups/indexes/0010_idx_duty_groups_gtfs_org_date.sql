CREATE INDEX idx_duty_groups_gtfs_org_date ON public.duty_groups USING btree (gtfs_id, organization_id, operation_date) WHERE (NOT deleted);
