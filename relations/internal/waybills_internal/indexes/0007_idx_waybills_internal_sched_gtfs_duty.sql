-- Serves the scheduled-trip half of the applicable-service-types lookup, which
-- joins waybills_internal on schedule_trip_id and then filters by duty_date.
--
-- Every other index on this table leads with gtfs_id plus a conductor, driver,
-- vehicle or waybill_no column, so that join has no usable index at all and
-- falls back to scanning the table. It only starts winning once the table is
-- large enough that a hash join stops being the cheaper plan.
--
-- duty_date trails the join keys because it is a range, and service_type is
-- INCLUDEd so the scan never has to visit the heap: those two columns are the
-- entire payload the lookup wants.
--
-- CONCURRENTLY: cannot run inside a transaction block.
CREATE INDEX CONCURRENTLY idx_waybills_internal_sched_gtfs_duty
    ON public.waybills_internal USING btree (gtfs_id, schedule_trip_id, duty_date)
    INCLUDE (service_type)
    WHERE (deleted = false);
