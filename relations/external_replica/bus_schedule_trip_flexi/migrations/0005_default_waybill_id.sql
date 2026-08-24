ALTER TABLE ONLY public.bus_schedule_trip_flexi ALTER COLUMN waybill_id SET DEFAULT nextval('public.bus_schedule_trip_flexi_waybill_id_seq'::regclass);
