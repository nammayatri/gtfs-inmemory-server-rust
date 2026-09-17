CREATE INDEX idx_trip_eta_override_active ON public.trip_eta_override USING btree (gtfs_id, expires_at);
