CREATE TABLE public.bus_shift_type_internal (
    shift_type_id text DEFAULT nextval('public.bus_shift_type_shift_type_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    shift_type_code character varying(255),
    description character varying(255),
    updated_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    gtfs_id text DEFAULT 'chennai_bus'::text
);
