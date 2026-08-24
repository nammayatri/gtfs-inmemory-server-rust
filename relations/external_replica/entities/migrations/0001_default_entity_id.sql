ALTER TABLE ONLY public.entities ALTER COLUMN entity_id SET DEFAULT nextval('public.entities_entity_id_seq'::regclass);
