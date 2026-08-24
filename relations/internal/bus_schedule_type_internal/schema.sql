CREATE TABLE public.bus_schedule_type_internal (
    schedule_type_id text NOT NULL,
    schedule_type_code text,
    schedule_type_name text,
    deleted boolean,
    created_at timestamp without time zone,
    updated_at timestamp without time zone,
    gtfs_id text
);
