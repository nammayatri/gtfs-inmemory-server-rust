CREATE TABLE public.entities (
    entity_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    entity_address character varying(255),
    entity_contact character varying(255),
    entity_email character varying(255),
    entity_name character varying(255) NOT NULL,
    entity_name_local_lang character varying(255),
    entity_remark character varying(255),
    entity_status character varying(255) NOT NULL,
    updated_at timestamp(6) without time zone,
    organization_id bigint NOT NULL
);
