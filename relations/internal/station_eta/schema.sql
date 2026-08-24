CREATE TABLE public.station_eta (
    gtfs_id character varying(255),
    source_station_code character varying(100),
    destination_station_code character varying(100),
    eta_in_seconds integer,
    created_at timestamp without time zone DEFAULT now(),
    updated_at timestamp without time zone DEFAULT now()
);
