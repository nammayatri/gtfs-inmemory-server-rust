ALTER TABLE ONLY public.employees ALTER COLUMN designation_id SET DEFAULT nextval('public.employees_designation_id_seq'::regclass);
