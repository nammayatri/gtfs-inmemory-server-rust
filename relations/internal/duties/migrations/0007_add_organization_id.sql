ALTER TABLE public.duties ADD COLUMN organization_id text;
UPDATE public.duties d SET organization_id = g.organization_id
FROM public.duty_groups g WHERE g.id = d.duty_group_id;
ALTER TABLE public.duties ALTER COLUMN organization_id SET NOT NULL;
CREATE TRIGGER duties_fill_org BEFORE INSERT ON public.duties
    FOR EACH ROW EXECUTE FUNCTION public.transit_fill_organization_id('duty_groups', 'id', 'duty_group_id');
