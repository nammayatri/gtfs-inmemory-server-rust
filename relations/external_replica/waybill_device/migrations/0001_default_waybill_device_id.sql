ALTER TABLE ONLY public.waybill_device ALTER COLUMN waybill_device_id SET DEFAULT nextval('public.waybill_device_waybill_device_id_seq'::regclass);
