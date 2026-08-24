CREATE TABLE public.bus_schedule_trip (
    schedule_trip_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    effective_end_date timestamp(6) without time zone,
    effective_start_date timestamp(6) without time zone,
    no_trip integer NOT NULL,
    schedule_number_name character varying(255),
    start_time character varying(255),
    status character varying(255),
    updated_at timestamp(6) without time zone,
    calendar_id bigint NOT NULL,
    schedule_id bigint NOT NULL
);
