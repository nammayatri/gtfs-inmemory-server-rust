ALTER TABLE public.duty_groups ADD COLUMN organization_id text;
UPDATE public.duty_groups d SET organization_id = e.organization_id
FROM public.entities_internal e WHERE e.entity_id = d.depot_id;
ALTER TABLE public.duty_groups ALTER COLUMN organization_id SET NOT NULL;
CREATE TRIGGER duty_groups_fill_org BEFORE INSERT ON public.duty_groups
    FOR EACH ROW EXECUTE FUNCTION public.transit_fill_organization_id(
        'entities_internal', 'entity_id', 'depot_id',
        'trip_groups', 'id', 'trip_group_id');
