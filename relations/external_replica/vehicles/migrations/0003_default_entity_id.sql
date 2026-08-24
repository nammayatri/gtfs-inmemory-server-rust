ALTER TABLE ONLY public.vehicles ALTER COLUMN entity_id SET DEFAULT nextval('public.vehicles_entity_id_seq'::regclass);
