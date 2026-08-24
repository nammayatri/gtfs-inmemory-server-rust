CREATE TABLE public.fleet_obu_mapping_internal (
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    updated_at timestamp(6) without time zone,
    fleet_no character varying(25),
    obu_serial_no character varying(150)
);
