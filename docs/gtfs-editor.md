# GTFS metadata editor

Ops edit a feed's **metadata** — stops, route stop order, route names and
polylines, and stations (stops clubbed under a parent) — in the internal DB. A
second person approves; commit applies the change and every GIMS pod reloads that
feed within seconds. Trip times are **not** editable: they keep coming from the
nightly GTFS build, which now reads the same tables.

```
ops ─VPN─▶ gtfs.sso.c2.integ.internal.svc.movingtech.net   (Pomerium, nammayatri.in)
             ├── /       → GIMS /internal/gtfs-editor/ui/     static dashboard
             └── /api/   → GIMS /internal/gtfs-editor/        JSON API below
                              │ commit = one transaction: apply changes,
                              │ gtfs_feed.version += 1, audit row
                              ▼
                 internal DB: gtfs_feed / gtfs_stop / gtfs_route / gtfs_route_stop
                              │
      GIMS pods poll gtfs_feed.version (5 s) ─┘ └─ nandi nightly build exports the
      and rebuild only the feed that moved         mapping, releases only if version moved
```

Schema: `db/gtfs_editor/0001..0003*.sql` (applied to master `mtc_internal_master`).

## 1. Data source per feed (GIMS loader)

Config (dhall, all optional so existing configs still parse):

```dhall
gtfs_db_feeds = ["chennai_bus"]      -- feeds whose metadata comes from the DB
gtfs_version_poll_seconds = 5
```

A DB feed is built from the tables **overlaid on the preprocessed data**, which
still supplies what the editor does not own: each route's trips (ids, directions)
and the example trip's start time, and the `trip_stoptimes/` shards behind
`/trip/{id}`.

For a DB feed, the loader produces exactly what `gtfs_preprocessor.py` would have
produced from a GTFS built out of these tables:

- **stops** — `gtfs_stop` rows that a served route row references, not deleted:
  `id = "{gtfs}:{stop_id}"`, `code`, `name`, `lat`, `lon`,
  `stationId = "{gtfs}:{parent_station}"`, `infoJson` (with `clusterId`),
  `locationType`, `platformCode`. Station rows (`location_type = 1`) are emitted
  too.
- **patterns** — one per route that has trips. Stops are the route's *served*
  rows (`stop_type` not in ROUTE CORRECTION / JUMP STOP / HIDDEN STOP) in
  `sequence` order, `stopSequence = i + 1`. Times follow the generator:
  `arrival = start + 135·i`, `departure = arrival + 15` (last stop:
  `departure = arrival`), where `start` is the preprocessed example trip's first
  arrival. `headsign` = stage number, or
  `{'fareStageNumber': 'N', 'isStageStop': true}` on NEW STOP rows. `trips` =
  the preprocessed pattern's trips.
- **routes** — routes with a pattern: `shortName`, `longName`, `mode` (from
  `route_type`), `agencyName` (`gtfs_feed.agency_name`), `color`,
  `encodedPolyline`, `tripCount`, `stopCount`, `startPoint`/`endPoint` from the
  pattern.

**Parity is the acceptance test**: loaded from the seeded tables, every public
static API for `chennai_bus` must return what the preprocessed load returns.

Reload: every `gtfs_version_poll_seconds`, `SELECT gtfs_id, version FROM
gtfs_feed WHERE gtfs_id = ANY($db_feeds)`. A feed whose version differs from the
one it was built at is rebuilt and swapped in (`ArcSwap`); others are untouched.
The version is read **before** the feed's rows, so an edit committed mid-load is
picked up on the next tick. A failed reload keeps the old data and retries.
`/version/{gtfs_id}` reflects the feed version.

## 2. Access

- **Pomerium** fronts the dashboard and API, and passes `X-Pomerium-Jwt-Assertion`.
  GIMS verifies it on every editor request: ES256 signature against the JWKS at
  `https://authenticate.c2.integ.internal.svc.movingtech.net/.well-known/pomerium/jwks.json`
  (cached, refreshed on unknown `kid`), `aud` = the dashboard host, `exp`. The
  email comes from the token — never from a header the client can set. The editor
  API therefore rejects requests that did not come through Pomerium, including any
  that reach GIMS via the public `/gtfs-inmemory/` virtual service. Every other
  GIMS API stays open.
- **Users** must exist in `gtfs_editor_user` with `status = active`. Emails in
  config `gtfs_editor_bootstrap_admins` are created as admins on first login.
- **2FA (TOTP, RFC 6238, SHA-1, 6 digits, 30 s, ±1 step).** The secret is
  encrypted with AES-256-GCM using `gtfs_editor_totp_key` (from the secrets dhall)
  and stored in `totp_secret_enc`. A code whose step is ≤ `totp_last_step` is
  rejected (no replay). 5 failed codes in 10 minutes lock sign-in for 10 minutes.
- **Session**: after a valid code, a random 32-byte token in cookie
  `gtfs_editor_session` (HttpOnly, Secure, SameSite=Strict, Path=/, 12 h); only its
  sha256 is stored. A request needs a valid Pomerium JWT **and** a session whose
  user has the JWT's email.
- **Mutations** also require header `X-Requested-With: gtfs-editor`.
- **Roles**: `viewer` read · `editor` drafts and submit · `approver` approve/reject/
  commit, **never their own submitted set** · `admin` everything plus users.
- Logs must never contain the JWT, cookie, TOTP code or secret (the request-header
  logger redacts `x-pomerium-jwt-assertion`, `cookie`, `token`, `authorization`).

## 3. API — `/internal/gtfs-editor`

JSON everywhere. Errors: `{"error": {"code": "...", "message": "...", "details": {...}}}`
with HTTP 400 validation · 401 no/invalid auth · 403 role · 404 · 409 conflict.
Lists take `limit` (default 50, max 500) and `cursor`; responses carry
`next_cursor`.

### Auth

| method | path | body → response |
|---|---|---|
| GET | `/auth/me` | → `{email, display_name, role, totp_enabled, session: bool}` (needs only the Pomerium JWT) |
| POST | `/auth/totp/enroll` | → `{otpauth_uri, secret_base32}` — only while `totp_enabled = false` |
| POST | `/auth/totp/confirm` | `{code}` → `{totp_enabled: true}` and starts a session |
| POST | `/auth/session` | `{code}` → sets cookie, `{expires_at}` |
| DELETE | `/auth/session` | → 204 |

### Live data (reads)

| method | path | notes |
|---|---|---|
| GET | `/feeds` | `[{gtfs_id, display_name, version, data_source, released_version}]` |
| GET | `/feeds/{g}/stops?q=&bbox=minLat,minLon,maxLat,maxLon&station=&limit=&cursor=` | `q` trigram on name, exact on id/code. Item: `{stop_id, stop_code, name, lat, lon, location_type, parent_station, platform_code, cluster_id, route_count, row_version}` |
| GET | `/feeds/{g}/stops/{stop_id}` | stop + `routes: [{route_id, short_name, long_name, sequence, stop_type, stage_no}]` + `children` (for a station) + `nearby` (≤ 60 m) |
| GET | `/feeds/{g}/routes?q=&limit=&cursor=` | `{route_id, short_name, long_name, color, has_polyline, stop_count, row_version}` |
| GET | `/feeds/{g}/routes/{route_id}` | route + `rows: [{sequence, stop_id, stop_name, lat, lon, stop_type, stage_no, stage_name, marker_id, marker_name, marker_lat, marker_lon}]` + `rows_hash` |
| GET | `/feeds/{g}/audit?change_set=&limit=&cursor=` | audit rows |

### Drafts

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/change-sets?status=` | list |
| POST | `/feeds/{g}/change-sets` | `{title, description?}` → draft, `base_version` = current feed version |
| GET | `/change-sets/{id}` | set + `changes` + `validation: [{change_id, level: error\|warning, code, message}]` + `conflicts` |
| POST | `/change-sets/{id}/changes` | append one change (below); draft only; editor+ |
| PUT | `/change-sets/{id}/changes/{change_id}` | replace a change's `after` |
| DELETE | `/change-sets/{id}/changes/{change_id}` | |
| GET | `/change-sets/{id}/preview/routes/{route_id}` | the route with the draft applied |
| POST | `/change-sets/{id}/submit` | draft → submitted; must have no validation errors |
| POST | `/change-sets/{id}/reopen` | submitted/rejected → draft (author or admin) |
| POST | `/change-sets/{id}/approve` | `{comment?}` submitted → approved; approver, ≠ submitter |
| POST | `/change-sets/{id}/reject` | `{comment}` submitted → rejected |
| POST | `/change-sets/{id}/commit` | approved → committed; approver/admin; see below |
| POST | `/change-sets/{id}/discard` | → discarded |

A change is `{entity, op, entity_key, after, base_row_version?}`:

| entity / op | `after` | validation |
|---|---|---|
| `stop` / `update` | any of `name, lat, lon, platform_code, cluster_id, regional_name, hindi_name` | lat/lon in range; a move > 500 m is a **warning** |
| `stop` / `create` | `{stop_id, name, lat, lon, stop_code?, platform_code?}` | id unused |
| `stop` / `delete` | `null` | error if any route row still uses it |
| `route` / `update` | any of `short_name, long_name, color, encoded_polyline, polyline_source` | colour `#RRGGBB`; polyline decodes |
| `route_stops` / `replace` | `{rows: [...], base_rows_hash}` — the whole ordered list | stop ids exist; first served row is NEW STOP; every INTERMEDIATE STOP carries the preceding NEW STOP's `stage_no` and `stage_name`; stage numbers never decrease; no stop twice in a row; ROUTE CORRECTION rows have a marker position |
| `station` / `create` | `{station_id, name, lat, lon, member_stop_ids: [...]}` | members exist, are location_type 0, have no other parent |
| `station` / `update` | `{name?, lat?, lon?, member_stop_ids?}` | same |
| `station` / `delete` | `null` | clears members' `parent_station` |

`POST /feeds/{g}/routes/{route_id}/polyline:osrm` returns a proposed polyline
through the route's served stops (not saved; add it as a `route` change).

**Commit** — one transaction: `SELECT … FROM gtfs_feed WHERE gtfs_id = $g FOR
UPDATE`; for each change in `position` order check the target's current
`row_version` (stop, route) or `rows_hash` (route_stops) against the change's base
— any mismatch aborts with 409 and lists the conflicts; apply; re-run validation
on the result; `version = version + 1`; set `committed_version`; audit. Nothing
is applied if anything fails.

### Admin

| method | path | |
|---|---|---|
| GET | `/users` | |
| POST | `/users` | `{email, display_name?, role}` |
| PATCH | `/users/{user_id}` | `{role?, status?}` — an admin cannot demote themselves |
| POST | `/users/{user_id}/reset-totp` | clears the secret; the user re-enrols |

Every mutation writes `gtfs_audit_log` with actor, action, set and a compact
detail.

## 4. Dashboard (`/internal/gtfs-editor/ui/`)

Static files served by GIMS, no build step, vendored libraries (no CDN). For the
ops team: search a stop or route, see it on a map, edit, collect edits into a
draft, submit, and — as a different person — review the diff and commit.
