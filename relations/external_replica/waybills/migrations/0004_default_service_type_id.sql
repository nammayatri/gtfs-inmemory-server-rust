ALTER TABLE ONLY public.waybills ALTER COLUMN service_type_id SET DEFAULT nextval('public.waybills_service_type_id_seq'::regclass);
