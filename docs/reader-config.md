# v1 and transitV2 reads, and the v1 → v2 cutoff

Writes to the transit tables live in tms-backend. GIMS serves the runtime reads from both internal
schedule models for every `gtfs_id`:

- **v1**: `waybills_internal` + `bus_schedule_trip_detail_internal` / `bus_schedule_trip_flexi_internal`
- **v2**: `trip_groups` / `trips` / `duty_groups` / `duties` (see `docs/transit-v2.md`)

A feed with no v2 data returns nothing from the v2 tables, so reading both changes nothing until a
feed is moved to v2.

## Order

| Reader | Order |
|---|---|
| `get_vehicle_data` | the bus's **online** v1 waybill, else its live v2 run (today or yesterday, IST), else its latest v1 waybill of any status |
| `get_vehicle_service_type` | the bus's **online** v1 waybill, else its latest v2 run (live first), else its latest v1 waybill |

Old v1 history never hides a live v2 run, and a bus still out on an online v1 waybill keeps it until
it closes. The route and trip schedule readers return rows from both models; v2 rows carry
`dutyTripId`.

## Cutoff

A feed listed in `v1_v2_cutoffs` reads v2 only from its cutoff date (IST). Before the date, and for
feeds that aren't listed, both models are read in the order above.

```dhall
v1_v2_cutoffs = Some [ { gtfs_id = "kolkata_bus", cutoff_date = "2026-11-22" } ],
```

- Optional; `None (List { gtfs_id : Text, cutoff_date : Text })` or leaving it out means no cutoff.
- `cutoff_date` is `YYYY-MM-DD`. A bad date, an empty `gtfs_id` or a feed listed twice stops startup.
- The date is checked on every read, so the switch happens at IST midnight without a restart.
- Set it to the day after the move's start date, so overnight v1 waybills from the day before can
  finish.
- The v1 waybill repeater automation stops for the feed from the cutoff.

Readers that follow it (`src/services/db_vehicle_reader_internal.rs`):

| Reader | Used by | v2 | v1 (before the cutoff) |
|---|---|---|---|
| `is_vehicle_in_internal` | `/vehicle/.../service-type`, `/vehicle/.../metadata` | a live run | `vehicles_internal` |
| `get_vehicle_data` | `/vehicle/.../service-type` | live run's trips | waybill + trip detail / flexi |
| `get_vehicle_service_type` | `/vehicle/.../metadata` | latest run's service type (live first) | latest waybill's service type |
| `get_waybills_by_route_id` | `/bus-route-schedule` | pending trips of live runs (appended after v1 rows) | waybill rows |
| `get_waybill_by_waybill_and_trip` | `/bus-trip-schedule` | the run's trip | the waybill's trip |
| `get_waybill_metadata` | `/waybill/{gtfs}/metadata/{waybill}` | the run | the waybill |

The v1 waybill repeater (`REPEATER_AUTOMATION_ENABLED_GTFS_IDS` in `src/services/operator.rs`) only
runs for a listed `gtfs_id` before its cutoff: the reconciler tick, the go-online fallback in v1
`currentOperation` and auto-close / successor on the last trip.

Not affected:

- The external replica (`database_url`, MTC) path and its switches `INTERNAL_ONLY_GTFS_IDS` /
  `EXTERNAL_ONLY_GTFS_IDS`, which choose between the replica and the internal DB.
- `SUPPORTED_OPERATOR_GTFS_IDS`, which still decides which feeds have operator APIs, internal
  route service tiers and the tag-number DB lookup.
- `station_eta`, which isn't part of either model.
