CREATE TABLE public.designations (
    designation_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    designation_name character varying(255) NOT NULL,
    designation_remark character varying(255),
    designation_status character varying(255) NOT NULL,
    is_default smallint DEFAULT 0 NOT NULL,
    updated_at timestamp(6) without time zone
);
