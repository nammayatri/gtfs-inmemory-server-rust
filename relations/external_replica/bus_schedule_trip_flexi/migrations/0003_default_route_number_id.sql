ALTER TABLE ONLY public.bus_schedule_trip_flexi ALTER COLUMN route_number_id SET DEFAULT nextval('public.bus_schedule_trip_flexi_route_number_id_seq'::regclass);
