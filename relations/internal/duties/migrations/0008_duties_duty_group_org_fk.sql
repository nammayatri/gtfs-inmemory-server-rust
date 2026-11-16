ALTER TABLE ONLY public.duties
    ADD CONSTRAINT duties_duty_group_org_fk FOREIGN KEY (duty_group_id, organization_id) REFERENCES public.duty_groups(id, organization_id) ON UPDATE CASCADE;
