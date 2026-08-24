-- Serves the flexi half of the applicable-service-types lookup.
--
-- idx_bstf_route_schedule already locates a route's flexi trips, but it carries
-- schedule_trip_id, and a flexi trip reaches its waybill through waybill_id
-- instead -- so that index costs a heap fetch per trip, for every trip the route
-- has ever had rather than only those in the lookback window. INCLUDE makes the
-- scan index-only.
--
-- Both predicates are exactly the ones the lookup applies, so the planner can
-- prove the partial index is usable.
--
-- CONCURRENTLY: cannot run inside a transaction block.
CREATE INDEX CONCURRENTLY idx_bstf_route_gtfs_waybill
    ON public.bus_schedule_trip_flexi_internal USING btree (route_number_id, gtfs_id)
    INCLUDE (waybill_id)
    WHERE (deleted = false AND trip_type <> 'dead-trip');
