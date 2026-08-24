ALTER TABLE ONLY public.bus_route_point ALTER COLUMN route_points_id SET DEFAULT nextval('public.bus_route_point_route_points_id_seq'::regclass);
