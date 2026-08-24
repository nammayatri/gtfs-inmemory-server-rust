ALTER TABLE ONLY public.employees ALTER COLUMN entity_id SET DEFAULT nextval('public.employees_entity_id_seq'::regclass);
