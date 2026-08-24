ALTER TABLE ONLY public.bus_service_type ALTER COLUMN service_type_id SET DEFAULT nextval('public.bus_service_type_service_type_id_seq'::regclass);
