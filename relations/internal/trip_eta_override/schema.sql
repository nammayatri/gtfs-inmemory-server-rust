CREATE TABLE public.trip_eta_override (
    gtfs_id character varying(100) NOT NULL,
    waybill_id text NOT NULL,
    trip_number integer NOT NULL,
    variant_id text NOT NULL,
    expires_at timestamp(6) with time zone NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now() NOT NULL,
    updated_by text
);
