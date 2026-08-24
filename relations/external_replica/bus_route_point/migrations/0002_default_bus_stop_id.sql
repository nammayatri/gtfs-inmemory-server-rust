ALTER TABLE ONLY public.bus_route_point ALTER COLUMN bus_stop_id SET DEFAULT nextval('public.bus_route_point_bus_stop_id_seq'::regclass);
