CREATE TABLE public.departments_internal (
    department_id bigint DEFAULT nextval('public.departments_department_id_seq'::regclass) NOT NULL,
    created_at timestamp(6) with time zone DEFAULT now(),
    deleted boolean DEFAULT false NOT NULL,
    department_name character varying(255) NOT NULL,
    department_remark character varying(255),
    department_status character varying(255) DEFAULT 'active'::character varying NOT NULL,
    is_default smallint DEFAULT 0 NOT NULL,
    updated_at timestamp(6) with time zone DEFAULT now()
);
