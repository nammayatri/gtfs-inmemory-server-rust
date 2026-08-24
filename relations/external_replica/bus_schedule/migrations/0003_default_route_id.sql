ALTER TABLE ONLY public.bus_schedule ALTER COLUMN route_id SET DEFAULT nextval('public.bus_schedule_route_id_seq'::regclass);
