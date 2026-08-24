ALTER TABLE ONLY public.bus_schedule_trip ALTER COLUMN calendar_id SET DEFAULT nextval('public.bus_schedule_trip_calendar_id_seq'::regclass);
