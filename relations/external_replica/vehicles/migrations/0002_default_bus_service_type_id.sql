ALTER TABLE ONLY public.vehicles ALTER COLUMN bus_service_type_id SET DEFAULT nextval('public.vehicles_bus_service_type_id_seq'::regclass);
