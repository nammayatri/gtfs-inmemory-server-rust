ALTER TABLE ONLY public.bus_schedule ALTER COLUMN service_type_id SET DEFAULT nextval('public.bus_schedule_service_type_id_seq'::regclass);
