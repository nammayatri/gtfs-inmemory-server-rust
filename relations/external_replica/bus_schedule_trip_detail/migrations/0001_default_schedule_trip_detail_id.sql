ALTER TABLE ONLY public.bus_schedule_trip_detail ALTER COLUMN schedule_trip_detail_id SET DEFAULT nextval('public.bus_schedule_trip_detail_schedule_trip_detail_id_seq'::regclass);
