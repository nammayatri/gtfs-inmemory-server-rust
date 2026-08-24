CREATE TABLE public.vehicles_internal (
    vehicle_id text DEFAULT nextval('public.vehicles_vehicle_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    fleet_no character varying(25),
    status character varying(50) DEFAULT 'active'::character varying,
    updated_at timestamp(6) with time zone DEFAULT now(),
    vehicle_no character varying(12),
    bus_service_type_id text DEFAULT nextval('public.vehicles_bus_service_type_id_seq'::regclass) NOT NULL,
    entity_id text DEFAULT nextval('public.vehicles_entity_id_seq'::regclass) NOT NULL,
    organization_id text DEFAULT nextval('public.vehicles_organization_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL,
    tag_number text
);
