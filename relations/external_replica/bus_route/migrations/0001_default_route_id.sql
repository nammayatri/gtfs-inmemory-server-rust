ALTER TABLE ONLY public.bus_route ALTER COLUMN route_id SET DEFAULT nextval('public.bus_route_route_id_seq'::regclass);
