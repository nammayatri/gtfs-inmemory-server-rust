CREATE TABLE public.bus_route_point (
    route_points_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    fare_stage character varying(5) DEFAULT 'N'::character varying,
    point_status character varying(50),
    route_order integer NOT NULL,
    stage_no integer,
    sub_stage character varying(5) DEFAULT 'N'::character varying,
    travel_distance integer NOT NULL,
    travel_time character varying(255),
    updated_at timestamp(6) without time zone,
    bus_stop_id bigint NOT NULL,
    route_id bigint NOT NULL
);
