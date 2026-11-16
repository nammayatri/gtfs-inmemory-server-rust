CREATE INDEX idx_duties_gtfs_org_time ON public.duties USING btree (gtfs_id, organization_id, scheduled_start_at) WHERE (NOT deleted);
