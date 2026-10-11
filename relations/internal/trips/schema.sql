CREATE TABLE public.trips (
    id                         TEXT NOT NULL,
    trip_group_id              TEXT NOT NULL,
    gtfs_id                    TEXT NOT NULL,
    operator_id                TEXT,
    route_id                   TEXT NOT NULL,
    is_bookable                BOOLEAN NOT NULL DEFAULT true,
    trip_number                INTEGER NOT NULL,
    trip_order                 INTEGER NOT NULL,
    scheduled_start_time       TIME NOT NULL,
    scheduled_start_day_offset SMALLINT NOT NULL DEFAULT 0,
    scheduled_end_time         TIME NOT NULL,
    scheduled_end_day_offset   SMALLINT NOT NULL DEFAULT 0,
    deleted                    BOOLEAN NOT NULL DEFAULT false,
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at                 TIMESTAMPTZ NOT NULL DEFAULT now()
);
