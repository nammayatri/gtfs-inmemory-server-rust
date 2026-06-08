clet LogLevel = < TRACE | DEBUG | INFO | WARN | ERROR | OFF >

let logger_cfg = {
    level = LogLevel.INFO,
    log_to_file = False
}

let secrets = ../secrets/gtfs_in_memory_server_rust.example.dhall

in {
  -- Logger configuration
  logger_cfg = logger_cfg,

  -- Database configuration
  database_url = None Text,
  internal_database_url = None Text,
  db_max_connections = 20,
  db_min_connections = 1,
  db_acquire_timeout = 5,
  db_idle_timeout = 600,
  db_max_lifetime = 3600,

  -- Cache configuration
  cache_duration = 3600,

  -- Trip filtering configuration
  ignored_trip_ids = ["t203"] : List Text,

  -- API configuration
  port = 8000,

  -- GTFS configuration
  polling_enabled = False,
  polling_interval = 10,
  process_batch_size = 100,
  gc_interval = 300,
  max_retries = 3,
  retry_delay = 5,
  rate_limit_delay = 0.1,
  cpu_threshold = 80.0,
  connection_limit = 100,
  memory_threshold = 1073741824,

  -- HTTP configuration
  http_pool_idle_timeout = 90,
  http_tcp_keepalive = 7200,
  dns_ttl = 300,

  -- OTP configuration
  otp_instances = {
    city_based_instances = [
      { url = "https://api.sandbox.moving.tech/nandi", identifier = "chennai_bus" }
    ],
    gtfs_id_based_instances = [] : List { identifier : Text, url : Text },
    default_instance = { url = "https://api.sandbox.moving.tech/nandi", identifier = "default" }
  },

  -- Bhubaneswar vehicle cache configuration
  bhubaneswar_cache_update_interval = 10,
  phone_number_hash_key = "HASH_KEY",

 enable_schedule_reconciliation=True,
  -- OSRTC station cache configuration
  osrtc_base_url = Some "OSRTC_BASE_URL",
  osrtc_username = secrets.osrtc_username,
  osrtc_secret_key = secrets.osrtc_secret_key,
  osrtc_station_refresh_interval_hours = 1,
  osrtc_feed_key = Some "odisha_osrtc",

    -- Preprocessed data configuration
  use_preprocessed_data = False,
  preprocessed_data_dir = "./assets",

  -- OSRM server for route polyline reprocessing (absent/empty ⇒ polyline skipped)
  osrm_url = Some "http://localhost:5050",
  gen_int_for_id = Some True,
}
