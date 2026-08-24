ALTER TABLE ONLY public.waybills ALTER COLUMN waybill_id SET DEFAULT nextval('public.waybills_waybill_id_seq'::regclass);
