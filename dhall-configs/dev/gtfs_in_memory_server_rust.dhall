let LogLevel = < TRACE | DEBUG | INFO | WARN | ERROR | OFF >

let logger_cfg = {
    level = LogLevel.INFO,
    log_to_file = False
}

let secrets = ../secrets/gtfs_in_memory_server_rust.example.dhall

-- Where the GTFS editor reads bus pings for "suggest a map line from GPS"
-- (docs/gtfs-editor.md section 17). Only url and user are required.
let GtfsGps =
      { url : Text
      , user : Text
      , table : Optional Text
      , days : Optional Natural
      , enough_bus_days : Optional Natural
      , feeds : Optional (List Text)
      , max_bus_days : Optional Natural
      , page_rows : Optional Natural
      , timeout_seconds : Optional Natural
      }

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

  -- Feeds whose stops/routes/stop order/polylines/stations come from the gtfs_*
  -- tables over internal_database_url (trips still from preprocessed data), and
  -- how often pods check gtfs_feed.version to reload them. Both optional.
  gtfs_db_feeds = [] : List Text,
  gtfs_version_poll_seconds = 5,

  -- How many days ahead the waybill repeater's hourly reconciler keeps `upcoming`
  -- waybills generated, for every operator in REPEATER_AUTOMATION_ENABLED_GTFS_IDS.
  repeater_lookahead_days = 7,
  repeater_tick_interval_secs = 300,
  repeater_min_run_interval_secs = 3600,

  -- Outbound webhooks (docs/gtfs-editor.md section 12). Off here: each pod
  -- reports the feed version it is serving only when this is True, and a
  -- webhook can fire only if its URL's host is in the allow-list, which is
  -- empty by default so the feature fails closed.
  --
  -- These two are only the SEED. Once an admin saves the policy from the
  -- dashboard, the gtfs_webhook_settings row supersedes both of them and
  -- editing here changes nothing (section 12.5) - the same way gtfs_feed.
  -- data_source supersedes gtfs_db_feeds above.
  gtfs_webhooks_enabled = False,
  gtfs_webhook_allowed_hosts = [] : List Text,
  -- Defaults to $POD_NAME, then the hostname. Two pods must never share it.
  gtfs_pod_id = None Text,

  -- OSRM server for route polyline reprocessing (absent/empty ⇒ polyline skipped)
  osrm_url = Some "http://localhost:5050",

  -- GPS pings for the editor's "suggest a map line from GPS". Off here: the
  -- endpoint answers 503 gps_unavailable. To turn it on, for example:
  --   gtfs_gps = Some
  --     { url = "https://clickhouse.internal:8443"
  --     , user = "gims_reader"
  --     , table = Some "atlas_kafka.amnex_direct_data"
  --     , days = Some 14
  --     , enough_bus_days = None Natural
  --     , feeds = Some [ "chennai_bus" ]
  --     , max_bus_days = None Natural
  --     , page_rows = None Natural
  --     , timeout_seconds = None Natural
  --     },
  -- The cluster is production and shared: GIMS reads it read-only (readonly=2
  -- on every query), one query at a time, and only bounded SELECTs.
  gtfs_gps = None GtfsGps,
  gtfs_gps_clickhouse_password = secrets.clickhouse_password,
  gen_int_for_id = Some True,
}
