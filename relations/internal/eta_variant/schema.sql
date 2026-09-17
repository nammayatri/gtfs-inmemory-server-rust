CREATE TABLE public.eta_variant (
    variant_id text NOT NULL,
    gtfs_id character varying(100) NOT NULL,
    code text NOT NULL,
    display_name text NOT NULL,
    is_default boolean DEFAULT false NOT NULL,
    deleted boolean DEFAULT false NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now() NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now() NOT NULL
);
