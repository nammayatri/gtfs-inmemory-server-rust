CREATE TABLE public.bus_schedule (
    schedule_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    effective_from timestamp(6) without time zone,
    effective_till timestamp(6) without time zone,
    route_code character varying(255),
    schedule_number character varying(255),
    service_code character varying(255),
    service_type_code character varying(255),
    schedule_type_code character varying(255),
    status character varying(50),
    updated_at timestamp(6) without time zone,
    entity_id bigint NOT NULL,
    route_id bigint NOT NULL,
    service_type_id bigint NOT NULL,
    schedule_type_id bigint NOT NULL
);
