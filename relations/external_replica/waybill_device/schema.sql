CREATE TABLE public.waybill_device (
    waybill_device_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    device_serial_no character varying(150),
    is_audited boolean DEFAULT false,
    is_primary boolean DEFAULT false,
    is_uploaded boolean DEFAULT false,
    updated_at timestamp(6) without time zone,
    waybill_id bigint NOT NULL
);
