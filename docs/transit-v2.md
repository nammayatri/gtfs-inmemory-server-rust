# transitV2 (trip groups, runs, duties)

Schedule model that replaces `bus_schedule_internal → bus_schedule_trip_internal →
bus_schedule_trip_detail_internal → waybills_internal` for feeds moved to it.

| Table | One row per |
|---|---|
| `trip_groups` | schedule / block (template) |
| `trips` | trip of a group (clock `TIME` + day offsets) |
| `duty_repeats` | repeat rule: weekdays, date window, default bus / crew |
| `duty_groups` | run = group × operation date; `waybill_no`, bus, default crew |
| `duties` | trip × operation date; crew per trip, `status` (upcoming / active / completed / skipped / cancelled) |
| `duty_event_logs` | crew / bus changes, trip status changes, generation failures |

Schema: `relations/internal/{trip_groups,trips,duty_repeats,duty_groups,duties,duty_event_logs}`.
`duty_groups/migrations/0001_btree_gist_extension.sql` needs a role that can create extensions.
Every row carries `gtfs_id` and `organization_id`.

## Writes

GIMS does not write these tables. All transit writes (v1 and v2), the daily duty generation job
and the driver trip actions live in tms-backend; dashboards and apps reach it through nammayatri.

## Reads

GIMS serves the runtime reads from both models for every feed, and from v2 only once a feed's
`v1_v2_cutoffs` date has passed (`docs/reader-config.md`):

| Endpoint | v2 source |
|---|---|
| `/vehicle/{gtfs_id}/service-type/{vehicle_no}` | the bus's live run: current trip, remaining trips |
| `/vehicle/{gtfs_id}/metadata/{vehicle_no}` | the latest run's service type (live first) |
| `/bus-route-schedule/{gtfs_id}/{route_id}` | pending trips of live runs, `upcoming` up to `maxDutyDate` |
| `/bus-trip-schedule/{gtfs_id}/{waybill_no}/{trip_number}/{route_id}` | the run's trip (`waybill_no` = `duty_groups.waybill_no`) |
| `/waybill/{gtfs_id}/metadata/{waybill_no}` | the run; driver of the given, running or next trip |

A live run is an active, not deleted `duty_groups` row operating today or yesterday in the feed's time zone (`gtfs_feed.timezone`, `Asia/Kolkata` when the feed has no row). Trip times are shown as HH:MM in that zone.
v2 rows carry `dutyTripId` = `<waybill_no>-<trip_number>`; v1 rows leave it empty.
