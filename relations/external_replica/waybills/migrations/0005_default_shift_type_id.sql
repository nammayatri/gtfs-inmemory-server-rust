ALTER TABLE ONLY public.waybills ALTER COLUMN shift_type_id SET DEFAULT nextval('public.waybills_shift_type_id_seq'::regclass);
