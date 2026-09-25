-- Every file of the GTFS reference in the editor's tables
-- (docs/gtfs-editor.md section 18).
--
-- Sections 1-16 hold a feed's stops, stations, routes, stop orders, trips, stop
-- times and calendars. This holds the rest of it, so that a feed's whole GTFS
-- lives here and the published zip is exported from these tables:
--
--   * the columns of stops.txt, routes.txt, trips.txt and stop_times.txt the
--     editor did not have (a stop's zone, level, URL and accessibility, a
--     route's URL, sort order and continuous stopping, a stop time's own
--     stop_sequence, distance and booking rules, a trip's cars_allowed);
--   * entrances, generic nodes and boarding areas (location_type 2-4);
--   * one table per remaining file, gtfs_<entity>, whose columns are the file's
--     own field names (src/gtfs/spec.rs is the registry): agency, feed_info,
--     shapes, levels, pathways, transfers, Fares v1 and v2, translations,
--     attributions, and the Flex files. A file with an id of its own is keyed by
--     it; one without (transfers.txt, fare_rules.txt, ...) by a minted row_id,
--     with its natural key still unique.
--
-- What the shipped feeds get wrong is kept as they have it, so their rows fit:
-- a stop that is its own parent (kolkata_metro, 55 stops), a trip calling at one
-- stop (chennai_bus, 150), a headway window left blank rather than 0. The
-- editor reports them; the table does not refuse them. References between the
-- new tables and the old are findings, not foreign keys: stops and routes are
-- soft-deleted and a route's trips are rewritten whole, and a key that refused
-- either would break editing that works today.
--
-- Every column added to an existing table is nullable or defaulted, and read
-- only where it is set, so every open draft's base hashes stay valid.
--
-- Goes on a database before the build that reads it, like 0012, 0017 and 0019:
-- the loader and the editor select the new columns. Safe to run twice.

BEGIN;

-- ------------------------------------------------------------ stops.txt
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS tts_stop_name       text;
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS zone_id             text COLLATE "C";
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS stop_url            text;
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS stop_timezone       text;
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS wheelchair_boarding smallint
    CHECK (wheelchair_boarding IN (0, 1, 2));
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS level_id            text COLLATE "C";
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS stop_access         smallint
    CHECK (stop_access IN (0, 1));
-- the stop's line in the stops.txt it came from: GIMS keeps the last stop it
-- reads of several that share a code, so the order is data
ALTER TABLE gtfs_stop ADD COLUMN IF NOT EXISTS sort_key            integer;

-- stations were 1; entrances, generic nodes and boarding areas come in
ALTER TABLE gtfs_stop DROP CONSTRAINT IF EXISTS gtfs_stop_location_type_check;
ALTER TABLE gtfs_stop ADD CONSTRAINT gtfs_stop_location_type_check
    CHECK (location_type BETWEEN 0 AND 4);
-- which location types may have which parent is a finding of the editor (an
-- entrance needs a station, a boarding area a platform); a stop may be its own
-- parent because a shipped feed has 55 that are
ALTER TABLE gtfs_stop DROP CONSTRAINT IF EXISTS gtfs_stop_check;
ALTER TABLE gtfs_stop DROP CONSTRAINT IF EXISTS gtfs_stop_check1;
CREATE INDEX IF NOT EXISTS gtfs_stop_zone_idx ON gtfs_stop (gtfs_id, zone_id) WHERE zone_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS gtfs_stop_level_idx ON gtfs_stop (gtfs_id, level_id) WHERE level_id IS NOT NULL;

-- ------------------------------------------------------------ routes.txt
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS route_desc          text;
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS route_url           text;
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS route_sort_order    integer
    CHECK (route_sort_order >= 0);
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS continuous_pickup   smallint
    CHECK (continuous_pickup BETWEEN 0 AND 3);
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS continuous_drop_off smallint
    CHECK (continuous_drop_off BETWEEN 0 AND 3);
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS network_id          text COLLATE "C";
ALTER TABLE gtfs_route ADD COLUMN IF NOT EXISTS sort_key            integer;

-- ------------------------------------------------------------ stop_times.txt
-- What a stop time says besides its time is its stop order's (section 16), so
-- these live on the stop order's rows. stop_sequence is kept only when the feed
-- numbers its stops otherwise than 1 to n; NULL is the row's place among the
-- served rows, which is what GIMS has always served.
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS stop_sequence            integer
    CHECK (stop_sequence >= 0);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS continuous_pickup        smallint
    CHECK (continuous_pickup BETWEEN 0 AND 3);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS continuous_drop_off      smallint
    CHECK (continuous_drop_off BETWEEN 0 AND 3);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS shape_dist_traveled      double precision
    CHECK (shape_dist_traveled >= 0);
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS pickup_booking_rule_id   text COLLATE "C";
ALTER TABLE gtfs_route_stop ADD COLUMN IF NOT EXISTS drop_off_booking_rule_id text COLLATE "C";

-- a trip may call at one stop: 150 of chennai_bus's do
ALTER TABLE gtfs_timing_profile DROP CONSTRAINT IF EXISTS gtfs_timing_profile_check;
ALTER TABLE gtfs_timing_profile ADD CONSTRAINT gtfs_timing_profile_check
    CHECK (cardinality(arrival_s) = cardinality(departure_s) AND cardinality(arrival_s) >= 1);

-- ------------------------------------------------------------ trips.txt, frequencies.txt
ALTER TABLE gtfs_trip ADD COLUMN IF NOT EXISTS cars_allowed smallint
    CHECK (cars_allowed IN (0, 1, 2));
-- blank is what most feeds write, and it is not the same cell as 0
ALTER TABLE gtfs_frequency ALTER COLUMN exact_times DROP NOT NULL;
ALTER TABLE gtfs_frequency ALTER COLUMN exact_times DROP DEFAULT;

-- ------------------------------------------------------------ the feed
-- 'served' (every feed so far): a DB feed serves the stops its routes call at,
-- and its stations. 'all': every stop and station, as the preprocessor does from
-- a feed's stops.txt; what a feed imported whole from its zip gets.
ALTER TABLE gtfs_feed ADD COLUMN IF NOT EXISTS stops_scope text NOT NULL DEFAULT 'served'
    CHECK (stops_scope IN ('served', 'all'));

-- ------------------------------------------------------------ the other files
-- agency.txt
CREATE TABLE IF NOT EXISTS gtfs_agency (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    agency_id                text COLLATE "C" NOT NULL,
    agency_name              text,
    agency_url               text,
    agency_timezone          text,
    agency_lang              text,
    agency_phone             text,
    agency_fare_url          text,
    agency_email             text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, agency_id)
);

-- fare_attributes.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_attribute (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    fare_id                  text COLLATE "C" NOT NULL,
    price                    double precision,
    currency_type            text,
    payment_method           integer,
    transfers                integer,
    agency_id                text COLLATE "C",
    transfer_duration        integer,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, fare_id)
);

-- fare_rules.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_rule (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    fare_id                  text COLLATE "C",
    route_id                 text COLLATE "C",
    origin_id                text COLLATE "C",
    destination_id           text COLLATE "C",
    contains_id              text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_fare_rule_natural_idx ON gtfs_fare_rule
    (gtfs_id, coalesce(fare_id::text, ''), coalesce(route_id::text, ''), coalesce(origin_id::text, ''), coalesce(destination_id::text, ''), coalesce(contains_id::text, ''));

-- timeframes.txt
CREATE TABLE IF NOT EXISTS gtfs_timeframe (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    timeframe_group_id       text COLLATE "C",
    start_time               integer,
    end_time                 integer,
    service_id               text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_timeframe_natural_idx ON gtfs_timeframe
    (gtfs_id, coalesce(timeframe_group_id::text, ''), coalesce(start_time::text, ''), coalesce(end_time::text, ''), coalesce(service_id::text, ''));

-- rider_categories.txt
CREATE TABLE IF NOT EXISTS gtfs_rider_category (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    rider_category_id        text COLLATE "C" NOT NULL,
    rider_category_name      text,
    is_default_fare_category integer,
    eligibility_url          text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, rider_category_id)
);

-- fare_media.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_media (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    fare_media_id            text COLLATE "C" NOT NULL,
    fare_media_name          text,
    fare_media_type          integer,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, fare_media_id)
);

-- fare_products.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_product (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    fare_product_id          text COLLATE "C",
    fare_product_name        text,
    rider_category_id        text COLLATE "C",
    fare_media_id            text COLLATE "C",
    amount                   text,
    currency                 text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_fare_product_natural_idx ON gtfs_fare_product
    (gtfs_id, coalesce(fare_product_id::text, ''), coalesce(rider_category_id::text, ''), coalesce(fare_media_id::text, ''));

-- fare_leg_rules.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_leg_rule (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    leg_group_id             text COLLATE "C",
    network_id               text COLLATE "C",
    from_area_id             text COLLATE "C",
    to_area_id               text COLLATE "C",
    from_timeframe_group_id  text COLLATE "C",
    to_timeframe_group_id    text COLLATE "C",
    fare_product_id          text COLLATE "C",
    rule_priority            integer,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_fare_leg_rule_natural_idx ON gtfs_fare_leg_rule
    (gtfs_id, coalesce(network_id::text, ''), coalesce(from_area_id::text, ''), coalesce(to_area_id::text, ''), coalesce(from_timeframe_group_id::text, ''), coalesce(to_timeframe_group_id::text, ''), coalesce(fare_product_id::text, ''));

-- fare_leg_join_rules.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_leg_join_rule (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    from_network_id          text COLLATE "C",
    to_network_id            text COLLATE "C",
    from_stop_id             text COLLATE "C",
    to_stop_id               text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_fare_leg_join_rule_natural_idx ON gtfs_fare_leg_join_rule
    (gtfs_id, coalesce(from_network_id::text, ''), coalesce(to_network_id::text, ''), coalesce(from_stop_id::text, ''), coalesce(to_stop_id::text, ''));

-- fare_transfer_rules.txt
CREATE TABLE IF NOT EXISTS gtfs_fare_transfer_rule (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    from_leg_group_id        text COLLATE "C",
    to_leg_group_id          text COLLATE "C",
    transfer_count           integer,
    duration_limit           integer,
    duration_limit_type      integer,
    fare_transfer_type       integer,
    fare_product_id          text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_fare_transfer_rule_natural_idx ON gtfs_fare_transfer_rule
    (gtfs_id, coalesce(from_leg_group_id::text, ''), coalesce(to_leg_group_id::text, ''), coalesce(fare_product_id::text, ''), coalesce(transfer_count::text, ''), coalesce(duration_limit::text, ''));

-- areas.txt
CREATE TABLE IF NOT EXISTS gtfs_area (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    area_id                  text COLLATE "C" NOT NULL,
    area_name                text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, area_id)
);

-- stop_areas.txt
CREATE TABLE IF NOT EXISTS gtfs_stop_area (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    area_id                  text COLLATE "C",
    stop_id                  text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_stop_area_natural_idx ON gtfs_stop_area
    (gtfs_id, coalesce(area_id::text, ''), coalesce(stop_id::text, ''));

-- networks.txt
CREATE TABLE IF NOT EXISTS gtfs_network (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    network_id               text COLLATE "C" NOT NULL,
    network_name             text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, network_id)
);

-- route_networks.txt
CREATE TABLE IF NOT EXISTS gtfs_route_network (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    network_id               text COLLATE "C",
    route_id                 text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_route_network_natural_idx ON gtfs_route_network
    (gtfs_id, coalesce(route_id::text, ''));

-- shapes.txt
CREATE TABLE IF NOT EXISTS gtfs_shape (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    shape_id                 text COLLATE "C" NOT NULL,
    -- the points, in order: shapes.txt's rows of one shape_id
    shape_pt_sequence        integer[]   NOT NULL,
    shape_pt_lat             double precision[] NOT NULL,
    shape_pt_lon             double precision[] NOT NULL,
    shape_dist_traveled      double precision[],     -- NULL: no point has one
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    CHECK (cardinality(shape_pt_sequence) = cardinality(shape_pt_lat)
           AND cardinality(shape_pt_lat) = cardinality(shape_pt_lon)
           AND (shape_dist_traveled IS NULL OR cardinality(shape_dist_traveled) = cardinality(shape_pt_lat))),
    PRIMARY KEY (gtfs_id, shape_id)
);

-- transfers.txt
CREATE TABLE IF NOT EXISTS gtfs_transfer (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    from_stop_id             text COLLATE "C",
    to_stop_id               text COLLATE "C",
    from_route_id            text COLLATE "C",
    to_route_id              text COLLATE "C",
    from_trip_id             text COLLATE "C",
    to_trip_id               text COLLATE "C",
    transfer_type            integer,
    min_transfer_time        integer,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_transfer_natural_idx ON gtfs_transfer
    (gtfs_id, coalesce(from_stop_id::text, ''), coalesce(to_stop_id::text, ''), coalesce(from_trip_id::text, ''), coalesce(to_trip_id::text, ''), coalesce(from_route_id::text, ''), coalesce(to_route_id::text, ''));

-- pathways.txt
CREATE TABLE IF NOT EXISTS gtfs_pathway (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    pathway_id               text COLLATE "C" NOT NULL,
    from_stop_id             text COLLATE "C",
    to_stop_id               text COLLATE "C",
    pathway_mode             integer,
    is_bidirectional         integer,
    length                   double precision,
    traversal_time           integer,
    stair_count              integer,
    max_slope                double precision,
    min_width                double precision,
    signposted_as            text,
    reversed_signposted_as   text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, pathway_id)
);

-- levels.txt
CREATE TABLE IF NOT EXISTS gtfs_level (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    level_id                 text COLLATE "C" NOT NULL,
    level_index              double precision,
    level_name               text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, level_id)
);

-- location_groups.txt
CREATE TABLE IF NOT EXISTS gtfs_location_group (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    location_group_id        text COLLATE "C" NOT NULL,
    location_group_name      text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, location_group_id)
);

-- location_group_stops.txt
CREATE TABLE IF NOT EXISTS gtfs_location_group_stop (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    location_group_id        text COLLATE "C",
    stop_id                  text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_location_group_stop_natural_idx ON gtfs_location_group_stop
    (gtfs_id, coalesce(location_group_id::text, ''), coalesce(stop_id::text, ''));

-- locations.geojson
CREATE TABLE IF NOT EXISTS gtfs_location (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    location_id              text COLLATE "C" NOT NULL,
    stop_name                text,
    stop_desc                text,
    geometry                 jsonb NOT NULL,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, location_id)
);

-- booking_rules.txt
CREATE TABLE IF NOT EXISTS gtfs_booking_rule (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    booking_rule_id          text COLLATE "C" NOT NULL,
    booking_type             integer,
    prior_notice_duration_min integer,
    prior_notice_duration_max integer,
    prior_notice_last_day    integer,
    prior_notice_last_time   integer,
    prior_notice_start_day   integer,
    prior_notice_start_time  integer,
    prior_notice_service_id  text COLLATE "C",
    message                  text,
    pickup_message           text,
    drop_off_message         text,
    phone_number             text,
    info_url                 text,
    booking_url              text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, booking_rule_id)
);

-- translations.txt
CREATE TABLE IF NOT EXISTS gtfs_translation (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    table_name               text,
    field_name               text,
    language                 text,
    translation              text,
    record_id                text,
    record_sub_id            text,
    field_value              text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_translation_natural_idx ON gtfs_translation
    (gtfs_id, coalesce(table_name::text, ''), coalesce(field_name::text, ''), coalesce(language::text, ''), coalesce(record_id::text, ''), coalesce(record_sub_id::text, ''), coalesce(field_value::text, ''));

-- feed_info.txt
CREATE TABLE IF NOT EXISTS gtfs_feed_info (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    feed_publisher_name      text,
    feed_publisher_url       text,
    feed_lang                text,
    default_lang             text,
    feed_start_date          date,
    feed_end_date            date,
    feed_version             text,
    feed_contact_email       text,
    feed_contact_url         text,
    feed_id                  text COLLATE "C",
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id)
);

-- attributions.txt
CREATE TABLE IF NOT EXISTS gtfs_attribution (
    gtfs_id      text COLLATE "C" NOT NULL REFERENCES gtfs_feed (gtfs_id),
    row_id       text COLLATE "C" NOT NULL,   -- minted: the file has no id of its own
    attribution_id           text COLLATE "C",
    agency_id                text COLLATE "C",
    route_id                 text COLLATE "C",
    trip_id                  text COLLATE "C",
    organization_name        text,
    is_producer              integer,
    is_operator              integer,
    is_authority             integer,
    attribution_url          text,
    attribution_email        text,
    attribution_phone        text,
    sort_key     integer,                   -- the row's place in the file it came from
    row_version  integer     NOT NULL DEFAULT 1,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    updated_by   text,
    PRIMARY KEY (gtfs_id, row_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS gtfs_attribution_natural_idx ON gtfs_attribution
    (gtfs_id, coalesce(attribution_id::text, ''), coalesce(agency_id::text, ''), coalesce(route_id::text, ''), coalesce(trip_id::text, ''), coalesce(organization_name::text, ''));

-- ------------------------------------------------------------ one agency per feed, to start
-- Until now a feed had one agency, its gtfs_feed.agency_name, and each route an
-- agency_id of free text. Every feed gets that agency as a row, under the id its
-- routes use most (blank when they use none). Its URL and timezone are not
-- known here and are not invented: the feed report says they are missing.
INSERT INTO gtfs_agency (gtfs_id, agency_id, agency_name, updated_by)
SELECT f.gtfs_id,
       coalesce((SELECT r.agency_id FROM gtfs_route r
                  WHERE r.gtfs_id = f.gtfs_id AND r.agency_id IS NOT NULL AND NOT r.deleted
                  GROUP BY r.agency_id ORDER BY count(*) DESC, r.agency_id LIMIT 1), ''),
       f.agency_name,
       '0023_gtfs_full_spec'
  FROM gtfs_feed f
 WHERE f.agency_name IS NOT NULL
   AND NOT EXISTS (SELECT 1 FROM gtfs_agency a WHERE a.gtfs_id = f.gtfs_id);

-- ------------------------------------------------------------ versions and drafts
-- Every table a change is based on carries a row_version. The trigger used to
-- name them, so a table added later silently lost its optimistic locking; it now
-- bumps the version of any row that has one.
CREATE OR REPLACE FUNCTION gtfs_touch_row() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF to_jsonb(NEW) ? 'row_version' THEN
        -- a commit whose base row_version no longer matches is a conflict,
        -- not an overwrite
        NEW.row_version := OLD.row_version + 1;
    END IF;
    NEW.updated_at := now();
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS gtfs_agency_touch ON gtfs_agency;
CREATE TRIGGER gtfs_agency_touch BEFORE UPDATE ON gtfs_agency
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_attribute_touch ON gtfs_fare_attribute;
CREATE TRIGGER gtfs_fare_attribute_touch BEFORE UPDATE ON gtfs_fare_attribute
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_rule_touch ON gtfs_fare_rule;
CREATE TRIGGER gtfs_fare_rule_touch BEFORE UPDATE ON gtfs_fare_rule
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_timeframe_touch ON gtfs_timeframe;
CREATE TRIGGER gtfs_timeframe_touch BEFORE UPDATE ON gtfs_timeframe
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_rider_category_touch ON gtfs_rider_category;
CREATE TRIGGER gtfs_rider_category_touch BEFORE UPDATE ON gtfs_rider_category
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_media_touch ON gtfs_fare_media;
CREATE TRIGGER gtfs_fare_media_touch BEFORE UPDATE ON gtfs_fare_media
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_product_touch ON gtfs_fare_product;
CREATE TRIGGER gtfs_fare_product_touch BEFORE UPDATE ON gtfs_fare_product
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_leg_rule_touch ON gtfs_fare_leg_rule;
CREATE TRIGGER gtfs_fare_leg_rule_touch BEFORE UPDATE ON gtfs_fare_leg_rule
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_leg_join_rule_touch ON gtfs_fare_leg_join_rule;
CREATE TRIGGER gtfs_fare_leg_join_rule_touch BEFORE UPDATE ON gtfs_fare_leg_join_rule
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_fare_transfer_rule_touch ON gtfs_fare_transfer_rule;
CREATE TRIGGER gtfs_fare_transfer_rule_touch BEFORE UPDATE ON gtfs_fare_transfer_rule
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_area_touch ON gtfs_area;
CREATE TRIGGER gtfs_area_touch BEFORE UPDATE ON gtfs_area
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_stop_area_touch ON gtfs_stop_area;
CREATE TRIGGER gtfs_stop_area_touch BEFORE UPDATE ON gtfs_stop_area
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_network_touch ON gtfs_network;
CREATE TRIGGER gtfs_network_touch BEFORE UPDATE ON gtfs_network
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_route_network_touch ON gtfs_route_network;
CREATE TRIGGER gtfs_route_network_touch BEFORE UPDATE ON gtfs_route_network
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_shape_touch ON gtfs_shape;
CREATE TRIGGER gtfs_shape_touch BEFORE UPDATE ON gtfs_shape
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_transfer_touch ON gtfs_transfer;
CREATE TRIGGER gtfs_transfer_touch BEFORE UPDATE ON gtfs_transfer
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_pathway_touch ON gtfs_pathway;
CREATE TRIGGER gtfs_pathway_touch BEFORE UPDATE ON gtfs_pathway
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_level_touch ON gtfs_level;
CREATE TRIGGER gtfs_level_touch BEFORE UPDATE ON gtfs_level
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_location_group_touch ON gtfs_location_group;
CREATE TRIGGER gtfs_location_group_touch BEFORE UPDATE ON gtfs_location_group
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_location_group_stop_touch ON gtfs_location_group_stop;
CREATE TRIGGER gtfs_location_group_stop_touch BEFORE UPDATE ON gtfs_location_group_stop
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_location_touch ON gtfs_location;
CREATE TRIGGER gtfs_location_touch BEFORE UPDATE ON gtfs_location
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_booking_rule_touch ON gtfs_booking_rule;
CREATE TRIGGER gtfs_booking_rule_touch BEFORE UPDATE ON gtfs_booking_rule
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_translation_touch ON gtfs_translation;
CREATE TRIGGER gtfs_translation_touch BEFORE UPDATE ON gtfs_translation
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_feed_info_touch ON gtfs_feed_info;
CREATE TRIGGER gtfs_feed_info_touch BEFORE UPDATE ON gtfs_feed_info
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();
DROP TRIGGER IF EXISTS gtfs_attribution_touch ON gtfs_attribution;
CREATE TRIGGER gtfs_attribution_touch BEFORE UPDATE ON gtfs_attribution
    FOR EACH ROW EXECUTE FUNCTION gtfs_touch_row();

ALTER TABLE gtfs_change DROP CONSTRAINT IF EXISTS gtfs_change_entity_check;
ALTER TABLE gtfs_change ADD CONSTRAINT gtfs_change_entity_check
    CHECK (entity IN (
        'stop', 'route', 'route_stops', 'station', 'feed_config', 'pattern',
        'timing_profile', 'route_trips', 'service', 'agency',
        'fare_attribute', 'fare_rule', 'timeframe', 'rider_category',
        'fare_media', 'fare_product', 'fare_leg_rule', 'fare_leg_join_rule',
        'fare_transfer_rule', 'area', 'stop_area', 'network', 'route_network',
        'shape', 'transfer', 'pathway', 'level', 'location_group',
        'location_group_stop', 'location', 'booking_rule', 'translation',
        'feed_info', 'attribution'));

COMMIT;
