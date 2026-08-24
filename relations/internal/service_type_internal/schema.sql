CREATE TABLE public.service_type_internal (
    service_type_id text DEFAULT nextval('public.bus_service_type_service_type_id_seq'::regclass) NOT NULL,
    abbreviation character varying(255),
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    service_type_code character varying(255),
    service_type_name character varying(255),
    status character varying(255) DEFAULT 'active'::character varying,
    ticket_footer character varying(255),
    ticket_footer_local_lang character varying(255),
    updated_at timestamp(6) with time zone DEFAULT now(),
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
