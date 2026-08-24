ALTER TABLE ONLY public.bus_schedule ALTER COLUMN schedule_id SET DEFAULT nextval('public.bus_schedule_schedule_id_seq'::regclass);
