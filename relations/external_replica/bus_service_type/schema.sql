CREATE TABLE public.bus_service_type (
    service_type_id bigint NOT NULL,
    abbreviation character varying(255),
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    service_type_code character varying(255),
    service_type_name character varying(255),
    status character varying(255),
    ticket_footer character varying(255),
    ticket_footer_local_lang character varying(255),
    updated_at timestamp(6) without time zone
);
