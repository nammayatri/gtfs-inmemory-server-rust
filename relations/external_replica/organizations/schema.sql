CREATE TABLE public.organizations (
    organization_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    organization_name character varying(255) NOT NULL,
    organization_name_local_lang character varying(255),
    organization_remark character varying(255),
    organization_short_name character varying(255),
    organization_short_name_local_lang character varying(255),
    organization_status character varying(255) NOT NULL,
    updated_at timestamp(6) without time zone
);
