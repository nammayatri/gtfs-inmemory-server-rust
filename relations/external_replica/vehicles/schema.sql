CREATE TABLE public.vehicles (
    vehicle_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    fleet_no character varying(25),
    status character varying(50),
    updated_at timestamp(6) without time zone,
    vehicle_no character varying(12),
    bus_service_type_id bigint NOT NULL,
    entity_id bigint NOT NULL,
    organization_id bigint NOT NULL
);
