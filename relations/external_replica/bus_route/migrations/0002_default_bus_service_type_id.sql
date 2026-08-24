ALTER TABLE ONLY public.bus_route ALTER COLUMN bus_service_type_id SET DEFAULT nextval('public.bus_route_bus_service_type_id_seq'::regclass);
