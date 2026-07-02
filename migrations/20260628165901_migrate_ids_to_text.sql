ALTER TABLE route_internal
  ALTER COLUMN route_id TYPE text USING route_id::text,
  ALTER COLUMN route_type_id TYPE text USING route_type_id::text,
  ALTER COLUMN bus_service_type_id TYPE text USING bus_service_type_id::text,
  ALTER COLUMN end_point_id TYPE text USING end_point_id::text,
  ALTER COLUMN start_point_id TYPE text USING start_point_id::text;

ALTER TABLE route_point_internal
  ALTER COLUMN route_points_id TYPE text USING route_points_id::text,
  ALTER COLUMN bus_stop_id TYPE text USING bus_stop_id::text,
  ALTER COLUMN route_id TYPE text USING route_id::text;

ALTER TABLE bus_schedule_internal
  ALTER COLUMN schedule_id TYPE text USING schedule_id::text,
  ALTER COLUMN entity_id TYPE text USING entity_id::text,
  ALTER COLUMN route_id TYPE text USING route_id::text,
  ALTER COLUMN service_type_id TYPE text USING service_type_id::text,
  ALTER COLUMN schedule_type_id TYPE text USING schedule_type_id::text;

ALTER TABLE bus_schedule_trip_internal
  ALTER COLUMN schedule_trip_id TYPE text USING schedule_trip_id::text,
  ALTER COLUMN calendar_id TYPE text USING calendar_id::text,
  ALTER COLUMN schedule_id TYPE text USING schedule_id::text;

ALTER TABLE bus_schedule_trip_detail_internal
  ALTER COLUMN schedule_trip_detail_id TYPE text USING schedule_trip_detail_id::text,
  ALTER COLUMN calendar_id TYPE text USING calendar_id::text,
  ALTER COLUMN route_number_id TYPE text USING route_number_id::text,
  ALTER COLUMN schedule_trip_id TYPE text USING schedule_trip_id::text;

ALTER TABLE bus_schedule_trip_flexi_internal
  ALTER COLUMN schedule_trip_flexi_id TYPE text USING schedule_trip_flexi_id::text,
  ALTER COLUMN calendar_id TYPE text USING calendar_id::text,
  ALTER COLUMN route_number_id TYPE text USING route_number_id::text,
  ALTER COLUMN schedule_trip_id TYPE text USING schedule_trip_id::text,
  ALTER COLUMN waybill_id TYPE text USING waybill_id::text;

ALTER TABLE service_type_internal
  ALTER COLUMN service_type_id TYPE text USING service_type_id::text;

ALTER TABLE stop_internal
  ALTER COLUMN bus_stop_id TYPE text USING bus_stop_id::text,
  ALTER COLUMN stop_group_id TYPE text USING stop_group_id::text,
  ALTER COLUMN stop_type_id TYPE text USING stop_type_id::text;

ALTER TABLE designations_internal
  ALTER COLUMN designation_id TYPE text USING designation_id::text;

ALTER TABLE employees_internal
  ALTER COLUMN emp_id TYPE text USING emp_id::text,
  ALTER COLUMN department_id TYPE text USING department_id::text,
  ALTER COLUMN designation_id TYPE text USING designation_id::text,
  ALTER COLUMN entity_id TYPE text USING entity_id::text,
  ALTER COLUMN organization_id TYPE text USING organization_id::text;

ALTER TABLE entities_internal
  ALTER COLUMN entity_id TYPE text USING entity_id::text,
  ALTER COLUMN organization_id TYPE text USING organization_id::text;

ALTER TABLE vehicles_internal
  ALTER COLUMN vehicle_id TYPE text USING vehicle_id::text,
  ALTER COLUMN bus_service_type_id TYPE text USING bus_service_type_id::text,
  ALTER COLUMN entity_id TYPE text USING entity_id::text,
  ALTER COLUMN organization_id TYPE text USING organization_id::text;

ALTER TABLE waybill_device_internal
  ALTER COLUMN waybill_device_id TYPE text USING waybill_device_id::text,
  ALTER COLUMN waybill_id TYPE text USING waybill_id::text;

ALTER TABLE fleet_etm_mapping_internal
  ALTER COLUMN fleet_etm_mapping_id TYPE text USING fleet_etm_mapping_id::text;

ALTER TABLE fleet_obu_mapping_internal
  ALTER COLUMN fleet_obu_mapping_id TYPE text USING fleet_obu_mapping_id::text;

ALTER TABLE bus_shift_type_internal
  ALTER COLUMN shift_type_id TYPE text USING shift_type_id::text;

ALTER TABLE bus_schedule_type_internal
  ALTER COLUMN schedule_type_id TYPE text USING schedule_type_id::text;

ALTER TABLE waybills_internal
  ALTER COLUMN waybill_id TYPE text USING waybill_id::text,
  ALTER COLUMN entity_id TYPE text USING entity_id::text,
  ALTER COLUMN schedule_id TYPE text USING schedule_id::text,
  ALTER COLUMN schedule_trip_id TYPE text USING schedule_trip_id::text,
  ALTER COLUMN service_type_id TYPE text USING service_type_id::text,
  ALTER COLUMN shift_type_id TYPE text USING shift_type_id::text;
