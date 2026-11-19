ALTER TABLE public.trip_groups ADD COLUMN organization_id text;
UPDATE public.trip_groups g SET organization_id = e.organization_id
FROM public.entities_internal e WHERE e.entity_id = g.depot_id;
ALTER TABLE public.trip_groups ALTER COLUMN organization_id SET NOT NULL;
CREATE TRIGGER trip_groups_fill_org BEFORE INSERT ON public.trip_groups
    FOR EACH ROW EXECUTE FUNCTION public.transit_fill_organization_id('entities_internal', 'entity_id', 'depot_id');
