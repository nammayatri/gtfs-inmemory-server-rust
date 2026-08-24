-- The flexi counterpart of idx_bstd_route_sched.
--
-- bus_schedule_trip_flexi_route_number_id_idx can seek by route but carries
-- nothing, so the lookup pays a Bitmap Heap Scan to fetch waybill_id for every
-- trip the route has ever had. Carrying waybill_id makes it index-only.
--
-- CONCURRENTLY: cannot run inside a transaction block.
CREATE INDEX CONCURRENTLY idx_bstf_route_waybill
    ON public.bus_schedule_trip_flexi USING btree (route_number_id, waybill_id)
    WHERE ((deleted = false) AND ((trip_type)::text <> 'dead-trip'::text));
