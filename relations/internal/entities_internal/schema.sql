CREATE TABLE public.entities_internal (
    entity_id text DEFAULT nextval('public.entities_entity_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    entity_address character varying(255),
    entity_contact character varying(255),
    entity_email character varying(255),
    entity_name character varying(255) NOT NULL,
    entity_name_local_lang character varying(255),
    entity_remark character varying(255),
    entity_status character varying(255) DEFAULT 'active'::character varying NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now(),
    organization_id text DEFAULT nextval('public.entities_organization_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
