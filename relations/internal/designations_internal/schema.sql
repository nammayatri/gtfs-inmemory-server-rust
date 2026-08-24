CREATE TABLE public.designations_internal (
    designation_id text DEFAULT nextval('public.designations_designation_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    designation_name character varying(255) NOT NULL,
    designation_remark character varying(255),
    designation_status character varying(255) DEFAULT 'active'::character varying NOT NULL,
    is_default smallint DEFAULT 0 NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now(),
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
