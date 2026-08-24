CREATE TABLE public.route_point_internal (
    route_points_id text DEFAULT nextval('public.bus_route_point_route_points_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    fare_stage character varying(5) DEFAULT 'N'::character varying,
    point_status character varying(50) DEFAULT 'active'::character varying,
    route_order integer NOT NULL,
    stage_no integer,
    sub_stage character varying(5) DEFAULT 'N'::character varying,
    travel_distance integer,
    travel_time character varying(255),
    updated_at timestamp(6) with time zone DEFAULT now(),
    bus_stop_id text DEFAULT nextval('public.bus_route_point_bus_stop_id_seq'::regclass) NOT NULL,
    route_id text DEFAULT nextval('public.bus_route_point_route_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL,
    stop_type text,
    stage_name text,
    is_visible boolean DEFAULT true
);
