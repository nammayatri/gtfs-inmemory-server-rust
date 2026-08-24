-- Serves the scheduled-trip half of the applicable-service-types lookup, which
-- looks trips up by route and needs only schedule_trip_id back.
--
-- Without it the planner picks idx_bstd_cover, which is
-- (schedule_trip_id, route_number_id, end_time): route_number_id is its SECOND
-- column, so a lookup by route alone cannot seek and scans the whole index --
-- measured at ~590 buffers across three parallel workers to return no rows.
-- It is chosen anyway because it is the only candidate that avoids the heap;
-- bus_schedule_trip_detail_route_number_id_idx can seek but carries nothing.
--
-- Leading with route_number_id and carrying schedule_trip_id gives the planner
-- one index that both seeks and covers, so it stops having to choose.
--
-- CONCURRENTLY: cannot run inside a transaction block.
CREATE INDEX CONCURRENTLY idx_bstd_route_sched
    ON public.bus_schedule_trip_detail USING btree (route_number_id, schedule_trip_id)
    WHERE ((deleted = false) AND ((trip_type)::text <> 'dead-trip'::text));
