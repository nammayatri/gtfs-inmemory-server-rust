ALTER TABLE ONLY public.employees
    ADD CONSTRAINT fke4i9i8vu1j96m71g4v98kqirb FOREIGN KEY (designation_id) REFERENCES public.designations(designation_id);
