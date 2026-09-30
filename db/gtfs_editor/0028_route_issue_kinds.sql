-- The kinds of reason a route is on the Routes to review queue.
--
-- 0026 allowed three. The stage mapper (scripts/map_mtc_stages.py) says more
-- than that about why a route's stages and MTC's do not line up, and refuses to
-- run while the table cannot hold what it would write:
--
--   absent            the replica does not carry the route
--   missing_internal  the replica carries it and our feed does not
--   count_differs     a different number of fare stages
--   name_differs      the same stages in the same places, spelled differently
--   order_differs     the same stages in another order
--   set_differs       neither the same set nor the same order
--   ambiguous_names   names repeat, so no stage can be identified safely
ALTER TABLE gtfs_route_stage_issue DROP CONSTRAINT IF EXISTS gtfs_route_stage_issue_issue_check;
ALTER TABLE gtfs_route_stage_issue ADD CONSTRAINT gtfs_route_stage_issue_issue_check
  CHECK (issue IN ('absent', 'missing_internal', 'count_differs', 'name_differs',
                   'order_differs', 'set_differs', 'ambiguous_names'));
