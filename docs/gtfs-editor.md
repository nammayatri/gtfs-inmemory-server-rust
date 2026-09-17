# GTFS metadata editor

Ops edit a feed's **metadata** — stops, route stop order, route names and
polylines, and stations (stops clubbed under a parent) — in the internal DB. A
second person approves; commit applies the change and every GIMS pod reloads that
feed within seconds. Trip times are **not** editable: they keep coming from the
nightly GTFS build, which now reads the same tables.

```
ops ─VPN─▶ <editor sign-in host>   (Pomerium, your SSO domain)
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

Schema: `db/gtfs_editor/0001..0008*.sql` (applied to master `mtc_internal_master`;
`0006` lets a change's `op` be `merge`, `0007` holds coordinate reviews, `0008`
makes `gtfs_feed.data_source` live and backfills `chennai_bus` to `'db'` — see
section 3's "Feed data source").

## 1. Data source per feed (GIMS loader)

Which feeds are DB-backed is a **live** setting, `gtfs_feed.data_source`
(`'db'` or `'preprocessed'`), not a fixed list decided at boot — see section
3's "Feed data source" for how it is read and changed. `gtfs_db_feeds` below
is now only a boot-time fallback, for a feed that has no `gtfs_feed` row yet:

```dhall
gtfs_db_feeds = ["chennai_bus"]      -- fallback only; a gtfs_feed row, once it
                                      -- exists, always overrides this list
gtfs_version_poll_seconds = 5        -- also how often the live data_source is polled
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
  your Pomerium's `/.well-known/pomerium/jwks.json` (`gtfs_editor_pomerium_jwks_url`
  in config; cached, refreshed on unknown `kid`), `aud` = the dashboard host
  (`gtfs_editor_audience`), `exp`/`nbf`. The
  email comes from the token — never from a header the client can set. The editor
  API therefore rejects requests that did not come through Pomerium, including any
  that reach GIMS via the public `/gtfs-inmemory/` virtual service. Every other
  GIMS API stays open.
- **Pomerium route**: `from: <editor sign-in host>` → the GIMS service, same
  paths (no prefix rewrite), `pass_identity_headers: true`.
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
`change_set_committed` (detail `{feed_version, changes, applied}`), and from
sections 5 and 6: `bulk_imported` (`{kind, rows, changes, first_change_id,
last_change_id}`), `stop_merged` (one per merge at commit: `{change_id, from, into,
routes, rows, keep_name, keep_position}`), `station_proposal_approved`,
`station_proposal_rejected`, `station_proposal_reopened`,
`station_proposal_returned` (`detail.reason` is `change_removed` or
`change_set_discarded`) and `station_proposal_committed`. Approving an area
writes one `station_proposal_approved` per station (`detail.bulk = true`). nandi's
scripts write `seed`, `release` and `station_proposals_built` (`{batch, proposals,
platforms, diameter_m, base_version}`). Coordinate reviews (section 8) write
`position_review_moved`, `position_review_split`, `position_review_confirmed`,
`position_review_reopened`, `position_review_returned` and
`position_review_committed`, and nandi's loader `position_reviews_loaded`. The
"Feed data source" endpoint below writes `feed_data_source_changed`, detail
`{gtfs_id, from, to}`. The dashboard's history has words for every one of these
(`ACTION_LABEL` in `editor-ui/js/admin.js`; `dev/ui_e2e.mjs` checks each action in
the database has a label).

### Feed data source

Whether GIMS serves a feed from these tables or from its preprocessed data —
section 1's `gtfs_feed.data_source` — read and changed here. **The POST here has
no relationship to a draft or change-set: it takes effect immediately**, for
everyone, without submit/approve/commit, unlike every other mutation in this
document. Every GIMS pod picks it up on its next `gtfs_version_poll_seconds`
poll (default 5 s) — no restart.

| method | path | body → response |
|---|---|---|
| GET | `/feeds/{g}/config` | viewer+ → `{gtfs_id, data_source: "db"\|"preprocessed", version}` — same vocabulary as `/feeds`' `data_source` (the raw `gtfs_feed.data_source` value). 404 `feed_not_found` if `g` has no `gtfs_feed` row at all |
| POST | `/feeds/{g}/config` | **admin only.** `{data_source: "db"\|"preprocessed"}` → the same shape as GET. 400 `invalid_data_source` for any other value. No row yet (first time this feed is switched to `db`) → inserts one, `version` starting at 1; a row already exists → updates `data_source` and bumps `version` the same way a change-set commit does. Audited as `feed_data_source_changed`, detail `{gtfs_id, from, to}` |


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
| `station` / `create` | `{station_id, name, lat, lon, member_stop_ids: [...]}` | `entity_key` = `station_id`; at least two members (`too_few_members`, refused when added: 400 `invalid_change`); members exist, are location_type 0, have no other parent |
| `station` / `update` | `{name?, lat?, lon?, member_stop_ids?}` | same; members, when sent, are at least two (to ungroup a station, delete it) |
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

A stop list that breaks only a fare or order rule (`fare_stage_mismatch`,
`first_stop_not_stage`, `intermediate_before_stage`, `stage_decreases`,
`stage_name_missing`, `stop_repeated`, `too_few_stops`, `route_empty`) still
applies in the draft: the preview and every later change in the set see the rows
as drafted, so the dashboard reopens what the person drafted, and the errors
block submit and commit. A stop that does not exist, is deleted or is a station,
or a row the table refuses, does not apply; the preview then shows the route
without that change.

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
GIMS, the dev Pomerium proxy, a local Postgres). The e2e signs in through the
gate, then drafts, approves and commits one flow per screen - the map (labels,
clicking stops over a route), stations to review, New stop and New route, the
stop list editor (Change stop, stage names, a fare error blocking submit), a
merge, a CSV import - and checks each commit in the database (psql) or the
public APIs. It needs a freshly seeded local database and changes it; put it
back afterwards. `--only map,stations,...` runs a subset.

## 5. Creating things, and bulk import (2026-09-17)

### New change types

| entity / op | `after` | validation |
|---|---|---|
| `stop` / `create` | `{stop_id?, name, lat, lon, stop_code?, platform_code?, regional_name?, hindi_name?}` | as above; **`stop_id` may be omitted**: the server mints `ed_` + 10 lower-case hex (unused) and returns it in the change (`entity_key` and `after.stop_id`) |
| `route` / `create` | `{route_id, short_name, long_name?, route_type?, color?, agency_id?}` | `entity_key` = `route_id`; id unused (409-style row error `route_exists`); same id charset as stops; `route_type` defaults to 3, `agency_id` to the feed's usual one; `short_name` required |
| `route` / `delete` | `null` | soft delete (`deleted = true`); refused while another pending change in the set edits the route |
| `station` / `create`, `update` | may carry `members: [{stop_id, platform_code?}]` instead of `member_stop_ids` | as before; `platform_code` ≤ 120 chars; a station change that came from a proposal carries `proposal_id` |
| `stop` / `merge` | `{into_stop_id, into_row_version, keep_name?: "into"\|"from", keep_position?: "into"\|"from"}` | see *Merging duplicate stops* below |

Settled while implementing:

- A create may leave out `entity_key`; it is taken from `after`'s id field (and
  the other way round). `entity_key` sent as `""` or `null` counts as left out -
  the dashboard's New stop sends `""` when no id is typed. A stop create with
  neither (or a blank / null `after.stop_id`) gets a minted id. An id is "unused"
  when no row of the feed has it (deleted rows included) and no open draft of the
  feed creates it.
- A station groups at least two stops. `station` / `create` and `update` with
  fewer members is 400 `invalid_change`, `details.code = "too_few_members"`, and the
  message says so ("station/create: a station groups at least two stops, and
  stn_x would have only one"); approving a proposal down to one stop is the
  `too_few_members` problem below. The dashboard checks the same before sending.
- `route` / `create`: the defaults are written into the stored change when it is
  added, so a reviewer sees `route_type` and `agency_id`. The feed's usual agency
  is the `agency_id` most of its routes carry. A colour is stored upper-case.
- `route` / `delete`: adding it while the set has a `route` or `route_stops`
  change for that route (a `route` / `create` included) is 409
  `route_has_pending_changes` with `details.change_ids`; an edit added after the
  delete makes the delete a validation error with the same code. A deleted
  route's rows stay; they no longer count as using a stop, so `stop` / `delete`
  of a stop only deleted routes call at is allowed.
- `members[].platform_code`: absent leaves the stop's label as it is; `null` (or
  blank) clears it. `member_stop_ids` and `members` are not sent together. The
  `before` of a station update or delete lists `members: [{stop_id,
  platform_code}]` beside `member_stop_ids`.

A new route's stop list is a `route_stops` / `replace` change on that route **in
the same draft**, positioned after the `route` / `create` (its `base_rows_hash` is
the hash of an empty list). Stops and routes created earlier in the same draft
count as existing for every later change's validation, preview and commit.

The hash of an empty list is
`4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945` (sha256 of
`[]`); it is also the `rows_hash` the draft preview of a new route returns. A
change to a stop or route created earlier in the same draft has no live row: its
`before` is the create's `after` (`[]` for a new route's stop list), it has no
`base_row_version`, and commit checks no version for it.

GIMS serves a route in its public APIs only once it has trips, and trips come
from the nightly GTFS build (MTC schedule). A route created here is in the
database at commit and in `/routes` after the next build gives it a schedule;
the dashboard says so on the create form and on the committed route.

### Merging duplicate stops

`stop` / `merge` merges a duplicate stop into another and moves every route row
to the kept id. The dashboard asks the user which id stays; the server does not
guess.

- `entity_key` = the stop that goes away (the "from" stop);
  `base_row_version` = its `row_version`; `after.into_stop_id` = the stop that
  stays, `after.into_row_version` = its `row_version` (filled from the live row
  when left out). `keep_name` and `keep_position` default to `"into"`.
- `before` (read shape) = `{from: <stop row>, into: <stop row>, affected:
  [{route_id, short_name, sequences: [...]}]}` (the live routes calling the from
  stop).
- **Errors**: both stops exist, are not deleted, have `location_type` 0 (never a
  station: `stop_is_station`), neither id starts with `prm_`
  (`merge_prm_stop`), and they are different ids (`merge_same_stop`);
  `merge_would_repeat_stop` when after the switch some live route would call the
  same stop on two consecutive served rows (row order by `sequence`; ROUTE
  CORRECTION, JUMP STOP and HIDDEN STOP rows are not served and do not separate
  two calls). The message names the route and both sequences - the MANALI RD.JN
  case, where route 56D has fare stage 1 on one id and stage 2 on the other, back
  to back. `stop_merged_away` on any later change in the same draft that uses the
  from stop (a stop update, delete or merge, a station's members, a
  `route_stops` row).
- **Warnings**: `merge_same_route_twice` (a route calls both stops, not back to
  back), `merge_far_apart` (more than 150 m apart; the message says the
  distance), `merge_names_differ`.
- **Apply** (preview replay and commit, the same code path):
  1. `UPDATE gtfs_route_stop SET stop_id = into WHERE gtfs_id = g AND stop_id =
     from`. Each row keeps its `stop_name_override`; a moved row without one gets
     the from stop's name when `keep_name` is `"into"` and the names differ, so the
     route keeps its own spelling. Provenance is kept, plus `{"merged_from": from}`.
  2. `keep_name` / `keep_position` `"from"` copy the from stop's name / lat+lon
     onto the kept stop.
  3. Station: when the from stop has a `parent_station` and the kept stop has
     none, the kept stop takes it and the from stop's `platform_code` (keeping its
     own label when the from stop has none).
  4. The from stop: `deleted = true`, `parent_station = NULL`, provenance
     `{"merged_into": into}`.
- **Commit** checks both `row_version`s; a mismatch is 409
  `change_set_conflicts` in the usual conflict shape (the kept stop's conflict has
  its id as `entity_key`). Commit audits `stop_merged` with the affected route
  and row counts. Route previews in the draft show the switched ids.

### Bulk import — preview, then add to a draft

`POST /change-sets/{id}/bulk` (editor+, draft only), body
`{kind, rows, dry_run}` with at most 5,000 rows:

| `kind` | row | becomes |
|---|---|---|
| `stops` | `{stop_id?, name, lat, lon, platform_code?}` | one `stop/create` per row |
| `routes` | `{route_id, short_name, long_name?, color?}` | one `route/create` per row |
| `route_stops` | `{route_id, sequence, stop_id, stop_type, stage_no, stage_name}` | one `route_stops/replace` per route (rows sorted by `sequence`) |

Response (both modes): `{dry_run, summary: {rows, ok, warnings, errors, changes},
rows: [{row, status: ok|warning|error, messages: [{code, message}], change:
{entity, op, entity_key}}], changes_preview: [...]}` — `row` is the 1-based
position in `rows`. Every row is validated exactly as the single change would be
(ids unused / existing, name and position, fare rules per route), plus duplicates
within the upload. `route_stops` may reference stops and routes created by an
earlier import into the same draft or by rows of this draft.

`dry_run: true` changes nothing. `dry_run: false` is refused (400
`bulk_has_errors`) if any row has an error; otherwise all changes are appended in
ONE transaction and the response carries the updated set. The draft is then
reviewed, submitted, approved and committed like any other.

Settled while implementing:

- Cells may be JSON numbers or text (a CSV cell is text): `lat`, `lon`,
  `sequence`, `stage_no` accept `"13.05"`; a blank or `null` optional cell counts
  as not given (a blank `stop_id` gets a minted id). A column that is not one of
  the kind's is `invalid_row`, as is a cell of the wrong type or a missing
  required one. 400 `invalid_kind`, `rows_required` (no rows) and `too_many_rows`
  (`details.max_rows`) refuse the whole request.
- `messages[]` also carry `level` (`error` / `warning`). `summary.changes` counts
  the changes the upload becomes (a row that cannot be read becomes none;
  `change` is `null` for it). In a dry run a stop without an id has `entity_key:
  null`; the real run mints the ids and adds `change_id` to each row's `change`,
  and the response carries the set as `change_set`. The 400 `bulk_has_errors`
  carries the same response in `details`.
- Duplicates (`duplicate_in_upload`, on every row involved): the same `stop_id`;
  the same stop name (any case) at the same position (6 decimals); the same
  `route_id` in `routes`; the same `(route_id, sequence)` in `route_stops`.
- `route_stops`: fare findings name rows by their `sequence`; a finding about
  the whole route (`route_not_found`, `route_deleted`, `too_few_stops`) is on
  every row of that route. A row calling at a stop the route already calls at
  keeps the route's own spelling (`stop_name_override`). Warnings:
  `markers_dropped` (the route has ROUTE CORRECTION rows, which a list of stops
  replaces) and `route_already_in_draft` (the draft already replaces that stop
  list). `base_rows_hash` is the live route's.
- Validation reads the draft's changes as applying cleanly (a draft whose
  changes do not apply shows it in its own validation and cannot be submitted).
  The real run audits one `bulk_imported`.

## 6. Station proposals — reviewed, then drafted, then released

`gtfs_station_proposal` (`db/gtfs_editor/0005_station_proposals.sql`) holds
stations suggested by nandi's `scripts/chennai-bus/editor/build_stations.py`
(same-named stops within 500 m; platforms labelled "Towards <next stop>").
Nothing is live until a person approves it into a draft and the draft is
committed through the normal maker-checker flow.

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/station-proposals?status=&bbox=&q=&limit=&cursor=` | `status` comma list (default `pending`); `bbox` minLat,minLon,maxLat,maxLon on the station point; `q` trigram on name, exact on station/member stop id. Item: `{proposal_id, station_id, name, lat, lon, spread_m, members: [{stop_id, name, lat, lon, platform_code, route_count}], status, change_set_id, change_set_title, reviewed_by_email, reviewed_at, review_note, batch}` |
| GET | `/station-proposals/{id}` | the item, with each member's CURRENT stop row (name, position, parent_station) and `problems: [{code, message}]` (e.g. a member now has a parent, was deleted, moved > 100 m) |
| GET | `/feeds/{g}/station-proposals/summary` | `{pending, approved, rejected, committed}` counts |
| POST | `/station-proposals/{id}/approve` | editor+. `{change_set_id, name?, lat?, lon?, members?: [{stop_id, platform_code}]}` — the reviewer may rename, move the station point, drop members or relabel platforms (never add a stop that is not a member). Appends one `station/create` change (`after.proposal_id` set) to the draft; proposal → `approved` with `change_set_id`/`change_id`. → the proposal detail (below). 400 with `details.problems` if the station would be invalid now |
| POST | `/feeds/{g}/station-proposals/approve` | editor+. `{change_set_id, proposal_ids: [...]}` — approve several unchanged, one transaction; `{results: [{proposal_id, ok, change_id?, problems?}]}`; proposals with problems are skipped, not fatal |
| POST | `/station-proposals/{id}/reject` | editor+. `{note}` (required) → `rejected` |
| POST | `/station-proposals/{id}/reopen` | editor+. rejected → pending |

Lifecycle, kept by the server: deleting the change from its draft, or discarding
the draft, puts the proposal back to `pending` (change fields cleared); committing
the draft marks it `committed`. Every transition writes `gtfs_audit_log`.

Settled while implementing:

- Items also carry `change_id`, `gtfs_id`, `created_at`, `updated_at`. `status`
  values are checked (400 `invalid_status`); `superseded` may be listed too. The
  summary counts only the four statuses above.
- Detail: each member gets `current` - `{name, lat, lon, parent_station,
  platform_code, location_type, deleted, row_version, route_count, moved_m}`, or
  `null` when the stop no longer exists. `problems[]` are `{level, code, stop_id?,
  message}`. Errors (the station would be invalid): `member_missing`,
  `member_deleted`, `member_is_station`, `member_has_parent` (another station; its
  own committed station is fine), `station_exists` (the id is taken; not reported
  once committed), `no_members`, `too_few_members` (the reviewer dropped all but
  one stop; not reported once committed), `stop_merged_away`. Warning:
  `member_moved` (> 100 m from where the build saw it).
- Approve checks against the live data plus the draft's changes (taken as
  applying), so a member another station in the same draft takes is a problem.
  Only errors refuse it (400 `proposal_has_problems`, `details.problems`); a
  moved member does not. `members[].platform_code` absent keeps the proposal's
  label, `null` clears the stop's label. Other refusals: 404
  `proposal_not_found` / `change_set_not_found`, 400 `feed_mismatch`, 409
  `change_set_not_draft`, 409 `proposal_not_pending`, 400
  `member_not_in_proposal` (`details.stop_id`), `invalid_members`,
  `invalid_platform_code`, `name_required`, `invalid_position`. The response is
  the proposal detail, now `approved` with `change_set_id` and `change_id`.
- Bulk approve skips a proposal with any problem, warnings included, and one
  that is not pending or not in the feed (`problems: [{code: proposal_not_pending
  | proposal_not_found}]`); two proposals of the request that share a stop or a
  station id cannot both be approved. At most 5,000 ids; the response adds
  `approved`, `skipped` and `change_set_id`.
- Reject is from `pending` only (409 `proposal_not_pending`; an approved one is
  taken out of its draft first); an empty note is 400 `note_required`. Reopen is
  from `rejected` only (409 `proposal_not_rejected`), clears the review fields,
  and is 409 `proposal_superseded` when another open proposal already suggests
  that station id. Both return the proposal detail.
- List, detail and summary are viewer+.

## 7. Dashboard requirements added 2026-09-17 (user feedback on master)

1. **Stations to review** page: proposal list (pending by default, search, counts
   by status, filter "in the map area"), map of the selected proposal — station
   point, every member kerb with its "Towards …" label, the routes through each
   kerb — editable name, point (drag), platform labels, drop a member; actions
   Approve into the current draft, Reject with a note, Approve all proposals in
   the map area (bulk endpoint); a proposal already in a draft links to it.
2. **Map**: in the focused area show every stop, each with its name label when
   zoomed in (labels at zoom ≥ 17, markers from zoom 15; station platforms of one
   station share one label). Clicking any stop marker switches the side panel to
   that stop, from any view, including while a route or another stop is open.
   Also fix: the search results dropdown must sit above the map controls; the map
   must `invalidateSize()` whenever the side panel changes width.
3. **Route stop list editor**: every row gets "Change stop" (search by name/id or
   pick on the map); "Add stop" is a labelled button between rows and at the end,
   not only a "+" icon; nothing about which stop a row is can be free text. A NEW
   STOP's stage name is chosen from the stop's own name or the route's existing
   stage names ("Other name…" only as an explicit choice); stage numbers can be
   renumbered in order with one action.
4. **Create**: a "New" menu — New stop (place by clicking the map, name, optional
   platform label; id minted by the server unless typed), New route (id, number,
   names, colour, then build its stop list with the stop pickers), New station
   (existing flow).
5. **Bulk import** page: choose Stops / Routes / Route stop lists, upload a CSV
   (downloadable template per kind; UTF-8, header row), parse in the browser,
   preview with `dry_run: true` — a table of rows with ok / warning / error and
   messages, a map of the stops, counts — fix the file and re-preview, then "Add N
   changes to draft". The draft then goes through the normal review.

## 8. Coordinate reviews — suspected wrong positions (2026-09-17)

`gtfs_position_review` (`db/gtfs_editor/0007_position_reviews.sql`) holds stops
whose coordinate is suspected wrong: nandi's
`scripts/chennai-bus/src/assets/review/coordinate_suspects.csv` (judged by agents
as carrying another stop's coordinate, or found off their own routes), loaded by
`scripts/chennai-bus/editor/load_position_reviews.py`. A reviewer checks each one
and either **moves** the stop — a `stop/update` change `{lat, lon, position_review_id}`
into a draft — or **splits** some of its routes onto a new stop (section 8.1), or
**confirms** the position is right, which closes it with no change. A stop can need
more than one of these: a busy stop can carry routes from several original places,
so a review may collect several moves and splits — never more than one move — in
one draft before it is released through the normal submit / approve (someone else)
/ commit.

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/position-reviews?status=&bbox=&q=&limit=&cursor=` | `status` comma list (default `pending`); `bbox` on the loaded position; `q` trigram on name, exact on stop id. Item: `{review_id, stop_id, original_stop_id, stop_name, reason, lat, lon, raw_lat, raw_lon, suggested_lat, suggested_lon, suggested_source, evidence, status, change_set_id, change_set_title, reviewed_by_email, reviewed_at, review_note, batch}` |
| GET | `/feeds/{g}/position-reviews/summary` | `{pending, approved, committed, confirmed}` |
| GET | `/position-reviews/{id}` | the item plus, computed now from the tables: `stop` (current row), `routes: [{route_id, short_name, sequence, prev: {stop_id, name, lat, lon} \| null, next: {...} \| null}]` (every route that calls at the stop, previous and next SERVED stop), `detour_m` (median over routes of d(prev,stop)+d(stop,next)−d(prev,next)), and `problems: [{code, message}]` (`stop_deleted`, `stop_merged_away`, `moved_since_load` when the stop is > 25 m from the loaded position) |
| POST | `/position-reviews/{id}/move` | editor+. `{change_set_id, lat, lon, note?}` → appends `stop/update` `{lat, lon, position_review_id}` (base_row_version = the stop's current) to the draft; review → `approved` (or stays `approved`, gaining this action, if it already has splits in the same draft). The response is the review detail, with `detour_m_after` for the new point |
| POST | `/position-reviews/{id}/confirm` | editor+. `{note?}` pending → `confirmed` |
| POST | `/position-reviews/{id}/reopen` | editor+. confirmed → pending |

`stop/update` accepts an optional `position_review_id`. Lifecycle, as for station
proposals, but a review's draft can hold several of its changes at once (section
8.1): removing one leaves the rest and the review `approved`; removing the last one,
or discarding the draft, returns the review to `pending`; committing marks it
`committed`. Every action is audited (`position_review_moved`,
`position_review_split`, `position_review_confirmed`, `position_review_reopened`,
`position_review_committed`, `position_review_returned`).

Dashboard — "Coordinates to review" page: the list (pending first, counts, search,
"in the map area"); for the selected stop a map with the current point, the raw
MTC point, the suggestion (labelled with its source) and every route's previous →
stop → next legs with their detour; the stops sharing its point; the reason and
Chalo hint. Actions: drag the pin or click the map (or "Use suggestion" / "Use raw
point") to set a new position, see the detour it would give, "Add move to draft";
"Position is correct" (note optional); "Next".

Settled while implementing:

- Items also carry `change_id`, `gtfs_id`, `created_at`, `updated_at`. `status`
  values are checked (400 `invalid_status`); `superseded` may be listed too. The
  summary counts only the four statuses above. `q` is exact on `stop_id` or
  `original_stop_id`. The list orders an exact id match first, then name
  similarity, then status (pending, approved, committed, confirmed, superseded),
  then `review_id`. List, detail and summary are viewer+.
- Detail: `stop` is the stop row in read shape, or `null`. `routes` has one entry
  per call of a live (not deleted) route at the stop, in `route_id, sequence`
  order: a route calling twice is listed twice, and a JUMP STOP call counts. Each
  entry also carries `stop_type` and its own `detour_m` (`null` at either end of
  the route). `prev`/`next` skip ROUTE CORRECTION, JUMP STOP and HIDDEN STOP rows;
  their `name` is the stop's own. `detour_m` is the median over the calls that have
  both neighbours (`null` when none has), in metres to one decimal; a neighbour
  that is the reviewed stop itself moves with it. The route context is one query
  of key lookups: 16-20 ms at a stop on 319 routes.
- `problems[]` are `{level, code, message}`. Errors (the stop cannot be changed
  now): `stop_missing`, `stop_deleted`, `stop_merged_away` (the stop is deleted and
  its provenance names `merged_into`; on a move, also a merge in the draft), and
  `stop_is_station`. Warning: `moved_since_load` (not reported once committed: the
  review's own move is expected to move it).
- The detail also carries `draft_actions: [{kind: "move" | "split", change_id,
  lat, lon, new_stop_id?, route_ids?, detour_m_after}]`, one entry per action the
  review has in its current draft, in change order (empty without a draft): a
  split's `detour_m_after` is the median over its own `route_ids`, computed at its
  point; a move's is the median over the routes the review's splits in the same
  draft leave at the stop. `new_position`, `new_stop_id`, `split_route_ids` and
  `detour_m_after` are the same fields for the **latest** action (`new_position`
  `{lat, lon}`; `new_stop_id`/`split_route_ids` are `null` for a move; all four are
  `null` without a draft). `?lat=&lon=` (below) overrides `detour_m_after` only,
  not `draft_actions`.
- Before drafting anything, `GET /position-reviews/{id}?lat=&lon=` (optionally
  `&route_ids=a,b`) answers `detour_m_after` for that point, over those routes'
  calls or every call, and changes nothing - the detour a dragged pin would give.
  `lat` without `lon`, `route_ids` without a point, or a point out of range is 400
  `invalid_position`.
- `/move` and `/split` both admit a review that is `pending`, or `approved` with
  its existing changes in the body's own `change_set_id` (`may_add_action`); an
  `approved` review with its changes in a **different** draft is 409
  `review_in_other_draft` with `details.change_set_id`. A second `/move` on a
  review that already has a `stop/update` in this draft is 409 `draft_conflict`
  naming that change - edit it instead of adding another.
- Move refusals: 400 `invalid_position` (out of range, or `0,0`), 400 `invalid_json`
  (a field that is not in the body above), 404 `review_not_found` /
  `change_set_not_found`, 400 `feed_mismatch`, 409 `change_set_not_draft`, 409
  `review_not_pending` / `review_in_other_draft` (`details.status`,
  `details.change_set_id`), 409 `draft_conflict` (`details.change_ids`; a repeat
  move), 400 `review_has_problems` (`details.problems`; the live data plus the
  draft, taken as applying; only errors refuse), 400 `position_unchanged` (within
  0.5 m of where the stop is: confirm instead). A move keeps the review's earlier
  note unless this call brings its own. The change's `before` is the stop row.
- Confirm is from `pending` only (409 `review_not_pending`); the body may be left
  out, and a blank note is none. Reopen is from `confirmed` only (409
  `review_not_confirmed`), clears the review fields, and is 409 `review_superseded`
  when another open review already covers the stop. Move, confirm and reopen
  return the review detail.
- A draft edit (`PUT /change-sets/{id}/changes/{change_id}`) of a change made for a
  review keeps its `position_review_id`: left out or `null`, it is put back;
  another id is 400 `invalid_change` with `details.code = "position_review_mismatch"`.
  A `stop/update` with `position_review_id` must move the stop (`lat` and `lon`),
  so a review's move cannot be edited into a rename. The id added to a change
  through `POST /change-sets/{id}/changes` is stored and ignored: only `/move` and
  `/split` tie a review to a draft.
- A review's `change_id` is its earliest remaining action's change (a move, or a
  split's `stop/create`); removing that change moves `change_id` on to whatever is
  now earliest, without touching the review's status. Back to pending -
  `change_set_id`, `change_id`, `reviewed_by`, `reviewed_at` and `review_note` are
  cleared; committed keeps the change's ids.
- Audit details: `position_review_moved` `{review_id, stop_id, change_id, from,
  to, moved_m, detour_m, detour_m_after, note}` (`detour_m`/`detour_m_after` over
  the routes the review's splits in that draft leave at the stop, not every call);
  `position_review_confirmed` `{review_id, stop_id, position, note}`;
  `position_review_reopened` `{review_id, stop_id, note}` (the note it had);
  `position_review_returned` `{review_id, stop_id, reason, removed_change_ids?}`
  (`reason` is `change_removed` or `change_set_discarded`; one entry per review
  actually emptied, not per change removed); `position_review_committed`
  `{review_id, stop_id, change_id, feed_version}` (one per review with changes in
  the committed draft, whatever their `change_id`).

### 8.1 Splitting routes off a stop

**Why.** The real data showed 54+ of the ~396 queued stops carry routes from more
than one original stop. The cleanup merged same-named stops on one (wrong) point
and moved routes onto the kerb stop they pass. Example: `29db2391b0` THIRUPORUR
THANDALAM serves 525/549/553K/553W/554B/565 of its own, plus 515/515A that came
from a Thiruporur-area stop and 592/593CT from a Thandalam near Athupakkam. Moving
that stop is wrong for most of its routes, so a reviewer must be able to split some
routes off onto a new stop.

**Evidence** (the loader writes this; the API returns `evidence` as stored):

- `evidence.route_groups: [{origin_stop_id, origin_name, suspect: bool, reason?,
  raw_lat?, raw_lon?, route_ids: [..], route_numbers: [..]}]`: which routes on the
  stop, at load time, came from which stop id in the published mapping.
- `evidence.mixed_origins: bool`: true when there is more than one group.
- Groups are a load-time snapshot. The detail's live `routes` stays authoritative.

**Endpoint.** `POST /position-reviews/{id}/split`, editor+.
Body: `{change_set_id, route_ids: [text], lat, lon, name?, note?}`.
It appends to the draft, in order:

1. `stop/create` `{name (default: the reviewed stop's current name), lat, lon,
   position_review_id}`. The id is left out so the server mints the `ed_` id as for
   any create.
2. For each route in `route_ids` order, one `route_stops` / `replace`: that route's
   current rows with EVERY row whose stop_id is the reviewed stop pointed at the new
   stop id. All other row fields stay unchanged, `base_rows_hash` is the current
   hash, and `after.position_review_id` is set.

Review → `approved` (or stays `approved`, gaining this action, if it already has a
move or other splits in the same draft), with `change_set_id` set and `change_id`
its earliest action's change (the stop/create, unless an earlier move or split is
already there). Response: the review detail, plus `new_stop_id` and
`detour_m_after` (median over the split routes, computed at the new point with
their prev/next served stops).

**Rules.**

- 400 `invalid_split` with `details.problems: [{code, message, route_id?}]` for:
  - `route_ids` empty or containing duplicates;
  - `route_not_at_stop`: the route doesn't currently call at the stop, or is
    deleted;
  - `would_empty_stop`: `route_ids` covers every non-deleted route that calls at
    the stop, so the reviewer should use /move;
  - lat/lon invalid by the same rules as stop create.
- 409 `draft_conflict` with `details.change_ids` when the draft already has a
  `route_stops` change for one of those routes, or a change to the reviewed stop
  that was **not** made for this review (its own earlier move, or a station change
  listing it as a member, do not conflict). `would_empty_stop` treats a route this
  same review already split off in this draft as gone too, so two splits that
  together take every route are refused on the second one, not silently allowed.
- The review must be `pending`, or `approved` with its changes already in this same
  draft (409 `review_in_other_draft` with `details.change_set_id` otherwise). The
  draft must be open and editable by the caller, same as /move.
- Returning to pending: removing a split's `stop/create` from the draft also
  removes that split's `route_stops` changes (the ones whose rows call its new
  stop id - a review's other split, or its move, is untouched). If the draft then
  holds no more of the review's changes, it goes back to `pending`; otherwise it
  stays `approved`, known by its earliest remaining action. Discarding the draft
  always returns every review it left `approved` to `pending`. Removing just one
  of a split's route changes (not its create) leaves the others, and the review
  stays approved with that split short one route.
- Committing marks the review `committed`, as for move.
- Audit `position_review_split`, detail `{new_stop_id, route_ids, change_set_id}`,
  one entry per split action (never merged across actions).
- The new stop inherits nothing else: no station, platform_code or codes.
- `route_stops/replace` accepts and ignores (stores) an optional
  `position_review_id` in `after`, the same way `stop/update` and `stop/create` do.

Settled while implementing:

- Codes in `invalid_split`: `route_ids_required`, `route_listed_twice` (once per
  route, with `route_id`), `invalid_position`, `route_not_at_stop` (with
  `route_id`), `would_empty_stop`. Route ids are trimmed. Checks run in this
  order, each stopping the request: the request's own problems; the draft and
  review as for move (404, 400 `feed_mismatch`, 409 `change_set_not_draft`, 409
  `review_not_pending`); 409 `draft_conflict`; 400 `review_has_problems` (the stop
  is missing, deleted, merged away or a station); then the route problems, listed
  together.
- "A change to the reviewed stop" is a `stop` change keyed on it (update, delete,
  or merging it away) or a `stop/merge` into it. A station change listing it as a
  member does not conflict: the new stop joins no station.
- A route calls at the stop on any row naming it, a JUMP STOP included; a deleted
  route never does. A blank `name` is the default. The new stop's `stop_code` is its
  minted id, as for any create; nothing is copied from the reviewed stop.
- Each `route_stops` change's `before` is the route's rows in read shape; its rows
  keep `stop_name_override` and `provider_id`. A problem a split row already had
  under the reviewed stop (a fare stage defect, the stop called twice in a row) is
  a warning "already present before this edit", as for any stop list, although
  the row now names the new stop: rows that differ from the live rows only in their
  stop are matched to their old selves. Anything else is graded as usual.
- Removing a split's `stop/create` removes only **that split's** stop lists - the
  draft's `route_stops` changes carrying the review's id whose rows call the
  removed stop's id - each cascade-audited as `change_removed` `{change_id,
  review_id, new_stop_id, reason: "split_removed"}`. A review's other split, or its
  move, is a separate `stop/create` or `stop/update` and is untouched. Discarding
  the draft returns every review it left `approved` to `pending` and leaves the
  discarded draft's changes as they were (a discarded draft never applies, and
  proposals' changes stay in theirs too).
- The detail of a split review (`draft_actions`, section 8's detail bullet): each
  split entry has `new_stop_id`; `route_ids`, that split's stop lists in draft
  order; `lat`/`lon`, the new stop's point; `detour_m_after` over those routes at
  that point (once committed, from their calls at the new stop, which is why a
  committed split's routes are no longer in the live `routes` - those are the
  reviewed stop's).
- Audit `position_review_split` detail also has `review_id`, `stop_id`,
  `change_id` (the split's own `stop/create`), `route_change_ids`, `name`, `lat`,
  `lon`, `detour_m_after` and `note`.

**Several actions on one review.** A stop can need more than one action - two
splits and a move, or two splits alone: `29db2391b0` THIRUPORUR THANDALAM needs a
Thiruporur split and a north-Thandalam split; KOLATHUR `bb3042f4cf` needs a 583D
split, a 500V split, and possibly a move for a kerb issue. Settled while
implementing this:

1. `/move` and `/split` accept a review that is `pending`, or `approved` with its
   `change_set_id` equal to the body's, in an editable draft (`may_add_action` in
   `src/editor/position_reviews.rs`). Approved in a **different** draft is 409
   `review_in_other_draft` with `details.change_set_id`.
2. The `draft_conflict` checks ignore changes carrying this review's own
   `position_review_id`, so a move plus later splits of the same stop coexist in
   one draft. A second `/move` on a review that already has a `stop/update` in
   this draft is 409 `draft_conflict`; edit that change in the draft instead of
   adding another. Splitting a route that already has a `route_stops` change in
   the draft is 409 `draft_conflict` too, which covers splitting the same route
   twice (including by a different review, and including re-splitting a route
   this same review already split off, though `would_empty_stop` would usually
   refuse that request first).
3. `would_empty_stop` counts the routes this review has already split off in this
   draft (`split_off`) as gone, on top of `route_ids`: two splits that between
   them would take every route are refused on the second call, naming
   `would_empty_stop` rather than letting the stop empty silently. A `route_ids`
   entry that repeats an already-split-off route is `route_not_at_stop` -
   dead code in practice, since `draft_conflict` (rule 2) refuses it first, but
   documented as the rule if that check is ever bypassed.
4. The review's `change_id` is the earliest remaining action's change (a move
   sorts by its `stop/update`'s position, a split by its `stop/create`'s;
   `first_change` falls back to the earliest change of any kind if no action
   remains but a stray stop list somehow does, which should not happen in
   practice). Removing one split's `stop/create` removes only that split's route
   changes - the ones whose rows call that new stop id - via `change_removed`. The
   review returns to `pending` only when the draft is left holding none of its
   changes (`first_change` is `None`); otherwise it stays `approved` and its
   `change_id` moves to the new earliest action if that changed. Discarding the
   draft returns it to pending as now, regardless of how many actions it had.
5. Detail keeps the singular fields `new_position`, `new_stop_id`,
   `split_route_ids` and `detour_m_after` as "the latest action" (last in change
   order), and adds `draft_actions: [{kind: "move" | "split", change_id, lat, lon,
   new_stop_id?, route_ids?, detour_m_after}]` for the review's current draft, in
   change order. Each split's `detour_m_after` is over its own `route_ids`; a
   move's is over the routes the review's splits in the same draft leave at the
   stop (so a move added after a split answers against what the stop still
   serves, not what it served before the split).
6. Audit: one entry per action, exactly as a single-action review already wrote
   (`position_review_moved` per move, `position_review_split` per split) - never
   merged into one entry for a review with several actions in one draft.
