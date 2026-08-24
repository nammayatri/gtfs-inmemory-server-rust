ALTER TABLE ONLY public.entities ALTER COLUMN organization_id SET DEFAULT nextval('public.entities_organization_id_seq'::regclass);
