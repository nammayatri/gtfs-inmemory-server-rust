ALTER TABLE public.trips ADD COLUMN organization_id text;
UPDATE public.trips t SET organization_id = g.organization_id
FROM public.trip_groups g WHERE g.id = t.trip_group_id;
ALTER TABLE public.trips ALTER COLUMN organization_id SET NOT NULL;
CREATE TRIGGER trips_fill_org BEFORE INSERT ON public.trips
    FOR EACH ROW EXECUTE FUNCTION public.transit_fill_organization_id('trip_groups', 'id', 'trip_group_id');
