let LogLevel = < TRACE | DEBUG | INFO | WARN | ERROR | OFF >

let logger_cfg = {
    level = LogLevel.INFO,
    log_to_file = False
}

in {
  -- Logger configuration
  logger_cfg = logger_cfg,

  -- Database configuration
  database_url = Some "psql://mtc_root_user:C%40uM7a%242025@13.234.6.205:5432/mtc_master_prod",
  db_max_connections = 20,
  db_min_connections = 1,
  db_acquire_timeout = 5,
  db_idle_timeout = 600,
  db_max_lifetime = 3600,

  -- Cache configuration
  cache_duration = 300,

  -- API configuration
  port = 8000,

  -- GTFS configuration
  polling_enabled = True,
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
      { url = "https://api.sandbox.moving.tech/nandi", identifier = "city1" }
    ],
    gtfs_id_based_instances = [] : List { identifier : Text, url : Text },
    default_instance = { url = "https://api.sandbox.moving.tech/nandi", identifier = "default" }
  }
}
