CREATE TABLE public.waybill_device_internal (
    waybill_device_id text DEFAULT nextval('public.waybill_device_waybill_device_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    device_serial_no character varying(150),
    is_audited boolean DEFAULT false,
    is_primary boolean DEFAULT false,
    is_uploaded boolean DEFAULT false,
    updated_at timestamp(6) with time zone DEFAULT now(),
    waybill_id text DEFAULT nextval('public.waybill_device_waybill_id_seq'::regclass) NOT NULL,
    gtfs_id character varying(100) DEFAULT 'chennai_bus'::character varying NOT NULL
);
