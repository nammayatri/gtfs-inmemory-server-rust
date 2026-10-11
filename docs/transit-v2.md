# transitV2 (trip groups, runs, repeat)

Schedule model that replaces `bus_schedule_internal → bus_schedule_trip_internal →
bus_schedule_trip_detail_internal → waybills_internal` for operators moved to it. Full design:
`scripts/plans/gims/transitV2/README.md` in the ny workspace.

| Table | One row per |
|---|---|
| `trip_groups` | schedule / block (template) |
| `trips` | trip of a group (clock `TIME` + day offsets) |
| `duty_repeats` | repeat rule: weekdays, date window, default bus / crew |
| `duty_groups` | run = group × operation date; `waybill_no`, bus, default crew |
| `duties` | trip × operation date; crew per trip, `status` (upcoming / active / completed / skipped / cancelled) + cancel / skip reason |
| `duty_event_logs` | crew / bus changes, every trip status change (with reason), generation failures |

Migrations: `relations/internal/{trip_groups,trips,duty_repeats,duty_groups,duties,duty_event_logs}`.
`duty_groups/migrations/0001_btree_gist_extension.sql` needs a role that can create extensions.

Code: `src/services/operator_v2.rs` (operator APIs, generation), `src/services/fleet_operator_v2.rs`
(driver APIs), handlers `v2_*` in `src/handlers/routes.rs`, dual-read in
`src/services/db_vehicle_reader_internal.rs`.

## APIs

Headers: `x-operator-id` (optional; scopes every read / write), `x-actor-person-id` (required on
writes; stored in `duty_event_logs`).

- `/internal/operator/{gtfs_id}/v2/` — `trip-groups`, `trip-groups/upsert`, `trip-groups/{id}`,
  `trip-groups/{id}/delete`, `trip-groups/{id}/trips`, `trip-groups/{id}/trips/upsert`,
  `trips/{id}/delete`, `duty-repeats`, `duty-repeats/upsert`, `duty-repeats/{id}/delete`,
  `duty-repeats/preview`, `duty-repeats/generate`, `duty-groups`, `duty-groups/create`,
  `duty-groups/{id}`, `duty-groups/{id}/duties`, `duty-groups/{id}/logs`,
  `duty-groups/{id}/vehicle`, `duty-groups/{id}/crew`, `duty-groups/{id}/active`,
  `duty-groups/{id}/delete`, `duties/{id}/crew`, `duties/{id}/delete`, `generation-failures`,
  `generation-failures/{id}/resolve`.
- `/internal/fleet-operator/{gtfs_id}/v2/` — `tripAction` (start / end / rollback / skip /
  cancel / uncancel), `currentOperation`, `activeTrip`.

Swagger: tag "Transit V2".

## Generation

`create run for (rule, date)` is idempotent (`UNIQUE (duty_repeat_id, operation_date)`, not
partial, so cancelled / deleted days are never recreated). A date is also skipped when a live
manual run (no `duty_repeat_id`) already operates the trip group that day (verdict `covered`), or
when every trip of that day has already ended (verdict `past`, e.g. saving a rule after today's
last trip). Triggers:

1. rule save → today .. today + 7 (skipped when the save sends `generate: false`; the dashboard leaves it unticked by default);
2. run finish (no pending trips after `end` / `skip` / `cancel`) → `operation_date + 7`;
3. daily k8s CronJob `k8s/transit-v2-duty-generate-cronjob.yaml` →
   `POST .../v2/duty-repeats/generate {"daysAhead": 7}` with `x-generation-trigger: CRON`;
4. manual `generate`.

A bus / crew clash creates the run with that field empty and logs `GENERATION_FAILURE`.

`/waybill/{gtfs_id}/metadata/{waybill_no}?tripNumber=N` returns trip N's crew for a transitV2 run
(crew is per trip); without it, the running / next trip's.

## Generic CRUD

`trip_groups`, `trips`, `duty_repeats`, `duty_groups`, `duties` are readable through
`/crud/{table}`. **Do not write them through CRUD**: it skips day-offset computation, run
creation (header + trips), windows, `waybill_no`, overlap handling and the audit log, and it
can't bind `TIME` / `DATE` / array columns. Use the `/v2` APIs.

## Tests

`TRANSIT_V2_TEST_DATABASE_URL=postgres://…@localhost/<db> cargo test --test transit_v2_flow`
(local DB with the transitV2 tables and, for the dual-read test, `vehicles_internal`,
`service_type_internal`, `entities_internal`, `employees_internal`, `route_internal`, waybill
tables).

## `dutyTripId`

Every GIMS response that carries a trip also has `dutyTripId` = `{waybill_no}-{trip_number}`.
It's the same value as the v2 fleet APIs' `tripId`, and it's unique per trip, unlike
`scheduleTripId`, which is the run (duty group) id on every trip of that run. It's set for
transitV2 data and `null` for waybill-model data (internal waybill tables, replica reader, CHALO cache).

| Response | Where |
|---|---|
| `/vehicle/{gtfs}/service-type/{veh}` and `/vehicle/{veh}/service-type` | top level (current trip) + each `remaining_trip_details[]` |
| `/bus-route-schedule`, `/bus-trip-schedule` | each row |
| `/waybill/{gtfs}/metadata/{waybill}` | the `?tripNumber=` trip, else the running / next trip |
| v2 fleet `currentOperation` / `tripAction` / `activeTrip` | each trip view (alongside `tripId`) |
| v2 operator duty group detail, duties list, trip crew update | each duty (`tripId` there is the template trip id) |
