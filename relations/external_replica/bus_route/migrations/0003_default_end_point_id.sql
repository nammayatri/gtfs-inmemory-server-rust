ALTER TABLE ONLY public.bus_route ALTER COLUMN end_point_id SET DEFAULT nextval('public.bus_route_end_point_id_seq'::regclass);
