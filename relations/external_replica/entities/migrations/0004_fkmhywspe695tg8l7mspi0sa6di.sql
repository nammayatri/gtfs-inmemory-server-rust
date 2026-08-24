ALTER TABLE ONLY public.entities
    ADD CONSTRAINT fkmhywspe695tg8l7mspi0sa6di FOREIGN KEY (organization_id) REFERENCES public.organizations(organization_id);
