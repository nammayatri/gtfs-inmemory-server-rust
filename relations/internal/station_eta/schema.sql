CREATE TABLE public.station_eta (
    gtfs_id character varying(255) NOT NULL,
    source_station_code character varying(100) NOT NULL,
    destination_station_code character varying(100) NOT NULL,
    eta_in_seconds integer NOT NULL,
    variant_id text NOT NULL,
    created_at timestamp without time zone DEFAULT now(),
    updated_at timestamp without time zone DEFAULT now()
);
