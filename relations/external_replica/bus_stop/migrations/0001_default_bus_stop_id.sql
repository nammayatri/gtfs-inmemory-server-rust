ALTER TABLE ONLY public.bus_stop ALTER COLUMN bus_stop_id SET DEFAULT nextval('public.bus_stop_bus_stop_id_seq'::regclass);
