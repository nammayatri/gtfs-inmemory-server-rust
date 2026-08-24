ALTER TABLE ONLY public.bus_schedule ALTER COLUMN entity_id SET DEFAULT nextval('public.bus_schedule_entity_id_seq'::regclass);
