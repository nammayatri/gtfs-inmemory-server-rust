ALTER TABLE ONLY public.bus_schedule_trip_detail ALTER COLUMN calendar_id SET DEFAULT nextval('public.bus_schedule_trip_detail_calendar_id_seq'::regclass);
