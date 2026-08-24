ALTER TABLE ONLY public.designations ALTER COLUMN designation_id SET DEFAULT nextval('public.designations_designation_id_seq'::regclass);
