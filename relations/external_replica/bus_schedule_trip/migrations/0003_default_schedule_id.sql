ALTER TABLE ONLY public.bus_schedule_trip ALTER COLUMN schedule_id SET DEFAULT nextval('public.bus_schedule_trip_schedule_id_seq'::regclass);
