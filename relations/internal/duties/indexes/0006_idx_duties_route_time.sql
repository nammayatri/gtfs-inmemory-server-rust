CREATE INDEX idx_duties_route_time ON public.duties USING btree (gtfs_id, route_id, scheduled_start_at) WHERE (status <> 'cancelled' AND NOT deleted);
