-- `code` is what control center shows and what operators type; constraining it here keeps
-- spaces, punctuation and case variants of the same intent ('AM Peak', 'am-peak', 'am_peak')
-- from becoming three distinct variants that nobody can tell apart in a dropdown.
--
-- Lowercase alphanumeric and underscore, starting with a letter: the shape the seeded codes
-- already use ('default', 'am_peak', 'monsoon').

ALTER TABLE ONLY public.eta_variant
    ADD CONSTRAINT eta_variant_code_check CHECK (code ~ '^[a-z][a-z0-9_]*$');
