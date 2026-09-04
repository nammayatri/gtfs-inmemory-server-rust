ALTER TABLE ONLY public.employees ALTER COLUMN organization_id SET DEFAULT nextval('public.employees_organization_id_seq'::regclass);
