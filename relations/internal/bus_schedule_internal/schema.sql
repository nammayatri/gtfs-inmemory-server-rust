CREATE TABLE public.bus_schedule_internal (
    schedule_id text DEFAULT nextval('public.bus_schedule_schedule_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    effective_from timestamp(6) with time zone,
    effective_till timestamp(6) with time zone,
    route_code character varying(255),
    schedule_number character varying(255),
    service_code character varying(255),
    service_type_code character varying(255),
    schedule_type_code character varying(255),
    status character varying(50) DEFAULT 'active'::character varying,
    updated_at timestamp(6) with time zone DEFAULT now(),
    entity_id text DEFAULT nextval('public.bus_schedule_entity_id_seq'::regclass) NOT NULL,
    route_id text DEFAULT nextval('public.bus_schedule_route_id_seq'::regclass) NOT NULL,
    service_type_id text DEFAULT nextval('public.bus_schedule_service_type_id_seq'::regclass) NOT NULL,
    schedule_type_id text DEFAULT nextval('public.bus_schedule_schedule_type_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
