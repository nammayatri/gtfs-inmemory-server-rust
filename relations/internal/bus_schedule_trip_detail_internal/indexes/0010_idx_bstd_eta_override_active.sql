-- Partial: only trips carrying an override are indexed, so the ops "what is in force" listing reads
-- a handful of rows. Keyed on gtfs_id alone -- nothing filters on the window: the overlap depends on
-- the joined waybill's duty_date, and the listing does not filter on the window having closed.
CREATE INDEX idx_bstd_eta_override_active ON public.bus_schedule_trip_detail_internal USING btree (gtfs_id) WHERE (eta_override_variant_id IS NOT NULL);
