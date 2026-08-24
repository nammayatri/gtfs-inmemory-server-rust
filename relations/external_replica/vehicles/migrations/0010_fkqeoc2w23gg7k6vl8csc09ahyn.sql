ALTER TABLE ONLY public.vehicles
    ADD CONSTRAINT fkqeoc2w23gg7k6vl8csc09ahyn FOREIGN KEY (organization_id) REFERENCES public.organizations(organization_id);
