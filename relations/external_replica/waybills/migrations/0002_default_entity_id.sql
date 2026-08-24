ALTER TABLE ONLY public.waybills ALTER COLUMN entity_id SET DEFAULT nextval('public.waybills_entity_id_seq'::regclass);
