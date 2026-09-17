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
picked up on the next tick. A failed reload keeps the old data and retries. A DB
feed that cannot load at boot serves its preprocessed data until a poll succeeds.
A snapshot boot rebuilds DB feeds on top of the snapshot, so a snapshot baked
without the DB never masks committed edits.

`/version/{gtfs_id}` for a DB feed is sha256 of the routes hash plus
`gtfs_feed.version`, so an edit to stops or stop order - invisible to the routes
hash - still moves it. It therefore differs from the preprocessed value for the
same data.

Acceptance: `scripts/parity_gtfs_db.py` compares two GIMS instances, one with the
feed in `gtfs_db_feeds` and one without.

## 2. Access

- **Pomerium** fronts the dashboard and API, and passes `X-Pomerium-Jwt-Assertion`.
  GIMS verifies it on every editor request: ES256 signature against the JWKS at
  `https://authenticate.c2.integ.internal.svc.movingtech.net/.well-known/pomerium/jwks.json`
  (cached, refreshed on unknown `kid`), `aud` = the dashboard host, `exp`/`nbf`. The
  email comes from the token — never from a header the client can set. The editor
  API therefore rejects requests that did not come through Pomerium, including any
  that reach GIMS via the public `/gtfs-inmemory/` virtual service. Every other
  GIMS API stays open.
- **Pomerium route**: `from: https://gtfs.sso.c2.integ.internal.svc.movingtech.net`
  → the GIMS service, same paths (no prefix rewrite), `pass_identity_headers: true`.
  On that host, `GET /` redirects to `/internal/gtfs-editor/ui/`; GIMS recognises
  the host from `X-Forwarded-Host` (or `Host` with `preserve_host_header: true`),
  so `/` on any other host is untouched.
- **Users** must exist in `gtfs_editor_user` with `status = active`. Emails in
  config `gtfs_editor_bootstrap_admins` are created as admins on first sight.
- **2FA (TOTP, RFC 6238, SHA-1, 6 digits, 30 s, ±1 step).** The secret is
  encrypted with AES-256-GCM using `gtfs_editor_totp_key` (from the secrets dhall)
  and stored in `totp_secret_enc`. A code whose step is ≤ `totp_last_step` is
  rejected (no replay). 5 failed codes in 10 minutes (since the last successful
  sign-in) lock sign-in for 10 minutes; failures are counted in the audit log, so
  the limit holds across pods (index `0004_audit_lockout_index.sql`).
- **Session**: after a valid code, a random 32-byte token in cookie
  `gtfs_editor_session` (HttpOnly, Secure, SameSite=Strict, Path=/, 12 h); only its
  sha256 is stored. A request needs a valid Pomerium JWT **and** a session whose
  user has the JWT's email.
- **Mutations** also require header `X-Requested-With: gtfs-editor`.
- **Roles are ordered** `viewer < editor < approver < admin`; each includes the
  ones below (an approver can also edit). `viewer` reads · `editor` drafts and
  submits · `approver` approves, rejects and commits — **never a set they
  submitted, neither approve nor commit** · `admin` everything plus users.
- Logs never contain the JWT, cookie, TOTP code or secret (the request-header
  logger redacts `x-pomerium-jwt-assertion`, `cookie`, `set-cookie`, `token`,
  `authorization`).
- **Local development**: `scripts/dev_pomerium_proxy.mjs` stands in for Pomerium
  (signs a JWT for a chosen email, writes its JWKS for a `file://` URL). It is a
  dev tool and must never be deployed. `editor-ui/dev/ui_e2e.mjs` drives the real
  dashboard through it against a local GIMS.

## 3. API — `/internal/gtfs-editor`

JSON everywhere. Errors: `{"error": {"code": "...", "message": "...", "details": {...}}}`
with HTTP 400 validation · 401 no/invalid identity or session · 403 role or
account · 404 · 409 conflict or wrong status · 429 locked. Every list is
`{items, next_cursor}`; paged lists take `limit` (default 50, max 500) and
`cursor`, and `next_cursor` is null on the last page.

### Sign-in errors the dashboard acts on

| status | code | meaning |
|---|---|---|
| 401 | `no_sso_identity` | no valid Pomerium assertion; `details.reason` is `jwt_missing`, `jwt_expired`, `jwt_wrong_audience`, `jwt_bad_signature`, … |
| 403 | `not_registered` | the SSO email has no editor account |
| 403 | `account_disabled` | the account is turned off |
| 401 | `totp_enrollment_required` | set up the authenticator first |
| 401 | `session_required` | enter a code (no session, or it ended) |
| 401 | `invalid_code` | wrong code; `details.attempts_left` before the lock |
| 401 | `code_reused` | that code's step was already used; `details.attempts_left` |
| 429 | `locked` | too many wrong codes; `details.retry_after_seconds` |

### Auth

| method | path | body → response |
|---|---|---|
| GET | `/auth/me` | → `{user_id, email, display_name, role, status, totp_enabled, session: bool, sign_in_locked_seconds}` (needs only the Pomerium JWT) |
| POST | `/auth/totp/enroll` | → `{otpauth_uri, secret_base32}` — only while `totp_enabled = false` |
| POST | `/auth/totp/confirm` | `{code}` → `{totp_enabled: true, expires_at}` and starts a session |
| POST | `/auth/session` | `{code}` → sets cookie, `{expires_at}` |
| DELETE | `/auth/session` | → 204 |

### Live data (reads)

| method | path | notes |
|---|---|---|
| GET | `/feeds` | `{items: [{gtfs_id, display_name, version, data_source, released_version, released_at, updated_at}], next_cursor: null}` |
| GET | `/feeds/{g}/stops?q=&bbox=minLat,minLon,maxLat,maxLon&station=&limit=&cursor=` | `q` trigram on name, exact on id/code. `station=true` lists stations only; `station=<id>` lists that station's platforms. Item: the stop row (`stop_id, stop_code, name, lat, lon, location_type, parent_station, platform_code, cluster_id, regional_name, hindi_name, info_json, position_source, provenance, deleted, row_version, updated_at, updated_by`) + `route_count` |
| GET | `/feeds/{g}/stops/{stop_id}` | the stop row + `route_count` + `routes: [{route_id, short_name, long_name, sequence, stop_type, stage_no}]` + `children` (a station's platforms, each with `route_count`) + `nearby` (≤ 60 m, nearest first, each with `distance_m` and `route_count`) + `parent` (the station row, or null) |
| GET | `/feeds/{g}/routes?q=&limit=&cursor=` | route row without the polyline + `has_polyline`, `stop_count` (served rows) |
| GET | `/feeds/{g}/routes/{route_id}` | route row (`route_id, short_name, long_name, route_type, agency_id, color, text_color, encoded_polyline, polyline_source, service_type, provenance, deleted, row_version, …`) + `stop_count` + `rows_hash` + `rows: [{sequence, stop_id, stop_name, lat, lon, stop_deleted, parent_station, stop_type, stage_no, stage_name, marker_id, marker_name, marker_lat, marker_lon, stop_name_override, provider_id}]`. `stop_name` is the route's own spelling when it has one (`stop_name_override`), else the stop's name |
| POST | `/feeds/{g}/routes/{route_id}/polyline:osrm?change_set=` | `{route_id, encoded_polyline, polyline_source: "osrm", waypoints, distance_m, saved: false}` — a proposal through the served stops and markers (of the draft, with `change_set`); not saved, add it as a `route` change |
| GET | `/feeds/{g}/audit?change_set=&limit=&cursor=` | newest first: `{audit_id, at, actor, actor_email, action, gtfs_id, change_set_id, detail}` |

Audit actions: `seed`, `release`, `user_bootstrapped`, `user_created`,
`user_updated`, `user_totp_reset`, `totp_enroll_started`, `totp_confirmed`,
`totp_failed`, `session_created`, `change_set_created`, `change_added`,
`change_updated`, `change_removed`, `change_set_submitted`, `change_set_approved`,
`change_set_rejected`, `change_set_reopened`, `change_set_discarded`,
`change_set_committed` (detail `{feed_version, changes, applied}`).

### Drafts

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/change-sets?status=` | `status` may be a comma list (`draft,submitted,approved`) |
| POST | `/feeds/{g}/change-sets` | `{title, description?}` → 201 draft, `base_version` = current feed version |
| GET | `/change-sets/{id}` | the set (`change_set_id, gtfs_id, title, description, status, created_by(_email), submitted_by(_email), reviewed_by(_email), committed_by(_email), *_at, review_comment, base_version, committed_version, change_count`) + `changes` + `validation: [{change_id, level: error\|warning, code, message}]` + `conflicts: [{change_id, entity, entity_key, reason, expected, actual, message}]` + `can_submit` + `stop_names` (`{stop_id: name}` for every stop a `route_stops` change references) |
| POST | `/change-sets/{id}/changes` | append one change (below); draft only; editor+ → 201 the set, plus `change_id` |
| PUT | `/change-sets/{id}/changes/{change_id}` | `{after, base_row_version?}` replaces a change's `after` → the set |
| DELETE | `/change-sets/{id}/changes/{change_id}` | → the set |
| GET | `/change-sets/{id}/preview/routes/{route_id}` | the route detail (same shape as above) with the draft applied, plus `validation` and `conflicts` |
| POST | `/change-sets/{id}/submit` | draft → submitted; 400 `validation_failed` with errors, 409 `change_set_conflicts` |
| POST | `/change-sets/{id}/reopen` | submitted/rejected/approved → draft; its creator, its submitter, or an admin |
| POST | `/change-sets/{id}/approve` | `{comment?}` submitted → approved; approver+, 403 `own_change_set` for the submitter |
| POST | `/change-sets/{id}/reject` | `{comment}` (required) submitted → rejected; same rule |
| POST | `/change-sets/{id}/commit` | approved → committed; approver+, 403 `own_change_set` for the submitter; see below |
| POST | `/change-sets/{id}/discard` | not committed/discarded → discarded; its creator or an admin |

Editing a set that is not a draft is 409 `change_set_not_draft`.

A change is `{entity, op, entity_key, after, base_row_version?}`. Its `before` is
a snapshot in read shape: the stop or route row; for a station, the row plus
`member_stop_ids`; for `route_stops`, the route detail's `rows` list.

| entity / op | `after` | validation |
|---|---|---|
| `stop` / `update` | any of `name, lat, lon, platform_code, cluster_id, regional_name, hindi_name` | lat/lon together and in range; a move > 500 m is a **warning** |
| `stop` / `create` | `{stop_id, name, lat, lon, stop_code?, platform_code?, cluster_id?, regional_name?, hindi_name?}` | `entity_key` = `stop_id`; id unused; ids are 1–64 of `A–Z a–z 0–9 _ - .` (no `:` — GIMS splits ids on it) |
| `stop` / `delete` | `null` | error if any route row still uses it |
| `route` / `update` | any of `short_name, long_name, color, text_color, encoded_polyline, polyline_source` | colour `#RRGGBB`; polyline decodes to ≥ 2 points |
| `route_stops` / `replace` | `{rows: [...], base_rows_hash}` — the whole ordered list | see below |
| `station` / `create` | `{station_id, name, lat, lon, member_stop_ids: [...]}` | `entity_key` = `station_id`; members exist, are location_type 0, have no other parent |
| `station` / `update` | `{name?, lat?, lon?, member_stop_ids?}` | same |
| `station` / `delete` | `null` | clears members' `parent_station` |

A `route_stops` row is `{stop_id, stop_type, stage_no, stage_name, marker_id?,
marker_name?, marker_lat?, marker_lon?, stop_name_override?, provider_id?}` —
nothing else (unknown fields are refused). Send `stop_name_override` and
`provider_id` back as the route detail gave them; a row without `provider_id`
takes the route's usual one. The per-row `provenance` of the cleanup is kept for
every row that is the same stop (or marker) at the same position. Rules: stop ids
exist and are not stations; the first served row is a NEW STOP; every
INTERMEDIATE STOP carries the preceding NEW STOP's `stage_no` and `stage_name`;
stage numbers never decrease; no stop twice in a row; ROUTE CORRECTION rows have a
marker position and no stop id. **Only problems the edit introduces block it**: a
problem the live route already had is reported as a warning ("already present
before this edit"), matched by code and stop, counted.

**Commit** — one transaction: `SELECT … FROM gtfs_feed WHERE gtfs_id = $g FOR
UPDATE`; for each change in `position` order check the target's current
`row_version` (stop, route) or `rows_hash` (route_stops) against the change's base
— any mismatch aborts with 409 `change_set_conflicts` and `details.conflicts`;
apply; re-run validation on the result; `version = version + 1`; set
`committed_version`; audit. Nothing is applied if anything fails.

### Admin

| method | path | |
|---|---|---|
| GET | `/users` | `{items: [{user_id, email, display_name, role, status, totp_enabled, created_at, last_login_at}], next_cursor: null}` |
| POST | `/users` | `{email, display_name?, role}` → 201 |
| PATCH | `/users/{user_id}` | `{role?, status?}` — an admin cannot demote or disable themselves |
| POST | `/users/{user_id}/reset-totp` | 204; clears the secret and ends the user's sessions; they re-enrol |

## 4. Dashboard (`/internal/gtfs-editor/ui/`)

Static files served by GIMS from `gtfs_editor_ui_dir` (`COPY editor-ui` in the
Dockerfile), no build step, vendored libraries (no CDN). The API base is
relative (`../`), so it works wherever the UI directory is mounted. For the ops
team: search a stop or route, see it on a map, edit, collect edits into a draft,
submit, and — as a different person — review the diff and commit.

Tests: `editor-ui/dev/mock_server.py` + `dev/ui_smoke.mjs` (UI against an
in-memory mock of this contract), and `dev/ui_e2e.mjs` (the real UI, the real
GIMS, the dev Pomerium proxy, a local Postgres).
