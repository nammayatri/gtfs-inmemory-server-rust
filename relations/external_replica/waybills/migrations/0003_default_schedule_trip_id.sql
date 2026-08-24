ALTER TABLE ONLY public.waybills ALTER COLUMN schedule_trip_id SET DEFAULT nextval('public.waybills_schedule_trip_id_seq'::regclass);
