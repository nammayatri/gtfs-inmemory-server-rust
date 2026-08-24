ALTER TABLE ONLY public.employees ALTER COLUMN department_id SET DEFAULT nextval('public.employees_department_id_seq'::regclass);
