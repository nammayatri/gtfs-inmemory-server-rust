CREATE TABLE public.bus_schedule_trip_internal (
    schedule_trip_id text DEFAULT nextval('public.bus_schedule_trip_schedule_trip_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    effective_end_date timestamp(6) with time zone,
    effective_start_date timestamp(6) with time zone,
    no_trip integer NOT NULL,
    schedule_number_name character varying(255),
    start_time character varying(255),
    status character varying(255) DEFAULT 'active'::character varying,
    updated_at timestamp(6) with time zone DEFAULT now(),
    calendar_id text DEFAULT nextval('public.bus_schedule_trip_calendar_id_seq'::regclass) NOT NULL,
    schedule_id text DEFAULT nextval('public.bus_schedule_trip_schedule_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
