ALTER TABLE ONLY public.bus_schedule ALTER COLUMN schedule_type_id SET DEFAULT nextval('public.bus_schedule_schedule_type_id_seq'::regclass);
