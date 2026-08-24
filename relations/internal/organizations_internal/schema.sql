CREATE TABLE public.organizations_internal (
    organization_id bigint DEFAULT nextval('public.organizations_organization_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    organization_name character varying(255) NOT NULL,
    organization_name_local_lang character varying(255),
    organization_remark character varying(255),
    organization_short_name character varying(255),
    organization_short_name_local_lang character varying(255),
    organization_status character varying(255) DEFAULT 'active'::character varying NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now()
);
