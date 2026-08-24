CREATE TABLE public.departments (
    department_id bigint NOT NULL,
    created_at timestamp(6) without time zone,
    deleted boolean DEFAULT false NOT NULL,
    department_name character varying(255) NOT NULL,
    department_remark character varying(255),
    department_status character varying(255) NOT NULL,
    is_default smallint DEFAULT 0 NOT NULL,
    updated_at timestamp(6) without time zone
);
