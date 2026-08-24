ALTER TABLE ONLY public.vehicles ALTER COLUMN organization_id SET DEFAULT nextval('public.vehicles_organization_id_seq'::regclass);
