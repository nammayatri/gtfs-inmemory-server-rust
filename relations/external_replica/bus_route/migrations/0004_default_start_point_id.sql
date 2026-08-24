ALTER TABLE ONLY public.bus_route ALTER COLUMN start_point_id SET DEFAULT nextval('public.bus_route_start_point_id_seq'::regclass);
