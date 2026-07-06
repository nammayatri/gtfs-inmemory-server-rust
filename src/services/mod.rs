/// The gtfs_id whose data lives in the **primary** Postgres pool
/// (`employees`, `entities`, `designations`). All other gtfs_ids are
/// served from the internal pool (`employees_internal`, `entities_internal`,
/// `designations_internal`). Used by fleet_operator login dispatch and
/// db_vehicle_reader depot lookup.
pub const PRIMARY_GTFS_ID: &str = "chennai_bus";

pub mod chalo_vehicle_cache;
pub mod db_employee_reader;
pub mod db_vehicle_reader;
pub mod db_vehicle_reader_internal;
pub mod field_generator;
pub mod fleet_operator;
pub mod gtfs_service;
pub mod operator;
pub mod osrtc_station_cache;
pub mod trip_service;
