CREATE TABLE public.vehicle_tag_internal (
    vehicle_tag_id text NOT NULL PRIMARY KEY,
    gtfs_id character varying(100) NOT NULL,
    tag text NOT NULL CHECK (tag <> '' AND tag = btrim(tag)),
    deleted boolean DEFAULT false NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    updated_at timestamp(6) with time zone DEFAULT now()
);
