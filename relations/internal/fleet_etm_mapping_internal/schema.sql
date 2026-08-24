CREATE TABLE public.fleet_etm_mapping_internal (
    fleet_etm_mapping_id text DEFAULT nextval('public.fleet_etm_mapping_internal_fleet_etm_mapping_id_seq'::regclass) NOT NULL,
    vehicle_no character varying(50) NOT NULL,
    gtfs_id character varying(50) NOT NULL,
    etm_serial_no character varying(100) NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL
);
