ALTER TABLE ONLY public.bus_schedule_trip_flexi ALTER COLUMN schedule_trip_flexi_id SET DEFAULT nextval('public.bus_schedule_trip_flexi_schedule_trip_flexi_id_seq'::regclass);
