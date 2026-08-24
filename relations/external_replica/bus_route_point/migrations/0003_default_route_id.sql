ALTER TABLE ONLY public.bus_route_point ALTER COLUMN route_id SET DEFAULT nextval('public.bus_route_point_route_id_seq'::regclass);
