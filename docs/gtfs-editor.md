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

Schema: `db/gtfs_editor/0001..0016*.sql` (`0001..0008` applied to master
`mtc_internal_master`; `0006` lets a change's `op` be `merge`, `0007` holds
coordinate reviews, `0008` makes `gtfs_feed.data_source` live and backfills
`chennai_bus` to `'db'` — see section 3's "Feed data source". **Not yet on
master, applied to the local database only:** `0009` lets a change's `entity` be
`feed_config`, `0010` adds `gtfs_change_set.self_approved` and relaxes the
maker-checker CHECK for a set so marked (section 2), `0011` adds the indexes
behind the cleanup context reads (section 9), `0012` adds the nullable
`gtfs_stop.description` (section 11), `0013` adds the webhook and pod cache
state tables (section 12), `0016` adds `gtfs_webhook_settings`, the webhook
policy row that supersedes the dhall config (section 12.5). All six are safe to
run twice. **`0012`
goes on a database before the build that reads it:** both the editor and the
GIMS loader select the column, so a DB feed fails to load (and serves its
preprocessed data) on a database without it.)

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
  `locationType`, `platformCode`, and `description` — **only on a stop that has
  one** (section 11), so a stop without is served exactly as the preprocessed
  load serves it. Station rows (`location_type = 1`) are emitted too.
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

### Merged-away stop ids keep answering

A DB feed emits live stops only, so committing a `stop/merge` used to make the
merged-away id 404 everywhere at once - while OTP was still running a GTFS in
which that id is live and handing it to the rider app. That is exactly what
happened on 2026-09-18: `stop/merge 2a25e7a0ed -> bd9b3af7c9` (Adyar Depot) made
`GET /stop/chennai_bus/2a25e7a0ed` and its route-stop mapping answer 404 `Stop
not found`, and `routeServiceability` in the rider app failed with 500
`UNABLE_TO_CALL_NANDI_GET_ROUTE_STOP_MAPPING_BY_STOP_CODE_API`. Anything holding
an id from before a merge - an OTP leg, a saved stop, a deep link, a GTFS-RT
join - hit the same wall.

So the loader also reads the rows a merge leaves behind. Alongside the live
stops it selects the **deleted** `gtfs_stop` rows whose
`provenance->>'merged_into'` is set, and builds an alias map, old code -> the
code of the stop that survived:

- **Chains are followed to the end.** A merged into B, B later into C, gives
  `A -> C` and `B -> C`. However many merges ago an id was retired, it lands on
  what survives today, never on an intermediate stop that is itself gone.
- **Both spellings are keys**: a retired stop's `stop_id` and its `stop_code`,
  because a caller holds whichever the feed served it. The value is always the
  survivor's public code - what the feed serves it under.
- **No alias is built** for a cycle (A into B, B back into A), for a chain whose
  end is deleted with no `merged_into` of its own (nothing survives to answer),
  or for a key that is live under either spelling (a live stop always wins, so a
  reused id is never shadowed). Those ids 404 exactly as they did before.

**The rule**: every public read that turns a stop code into a stop resolves it
through the alias map first - `get_stop`, the route-stop mappings by stop (plain,
`direction`, `allowClusters`), the `getAllStopsByIds` /
`getAllRouteStopMappingsByStopCodes` bulk reads, `station-children`,
`alternateStops`, and both cluster reads (`destinations`, `routes/{from}/{to}`).
`/stop-code/{g}/{provider}` resolves its *answer*, so a static provider mapping
naming a since-merged stop still hands back a live code. The response is the
**survivor's**, `stopCode` included: that is how a caller learns the new id.
Nothing is added to the body.

A single-stop read that was redirected also carries the header
`X-Stop-Alias: <old>=<new>`, so the redirect is observable without diffing the
body, and the redirect is logged at info at most once an hour per `(old, new)`
pair - enough to see which retired ids are still in callers' hands.

**Caveat**: the alias exists only while the deleted row keeps its `merged_into`
provenance. Hard-delete that row, or strip its provenance, and the old id goes
back to 404 - the alias map is derived from those rows on every load, nothing
else remembers the merge.

The map is rebuilt with the feed on **every** reload, so a merge committed in the
editor starts answering within one `gtfs_version_poll_seconds` tick, on the same
poll that brings the merge itself in. A preprocessed feed has no alias map at all
(it has no merges to know about), so reverting a feed out of DB mode returns it
to answering for live codes only, and parity is unaffected:
`scripts/parity_gtfs_db.py` samples codes out of `/stops/{g}`, which are all
live, and a live code is never an alias key.

Cost, measured on the local `chennai_bus` (7,855 served stops): the extra
deleted-row query is **0.6-3.2 ms** against a feed load of ~480-540 ms, and it
finds **0 merged-away stops** in the current seed, so the alias map is empty.
Forced to a worst case - a copy of those stops with 800 rows merged away in one
800-long chain - the query is 1.3-1.7 ms, all 800 aliases resolve to the one
survivor, and the load time does not move.

### A station code answers everywhere a stop code does

A feed's stops are grouped into **stations**: a station row has
`location_type = 1` and the places a bus actually calls at are its **platforms**,
each its own stop with `parent_station` set. Callers hold either kind of code -
an OTP leg, a saved stop, a deep link and a map pin are all just a stop code -
and cannot be expected to know which kind they have.

GIMS used to answer some endpoints for a station and quietly nothing for others.
`/route-stop-mapping/{g}/stop/{station}` already fanned out to the platforms, so
the rider app got the routes; `/cluster/{g}/destinations/{station}` walked from
the station itself, which no trip calls at, and returned `[]`. The screen showed
routes with no ETAs and no vehicles, and nothing said why - the code was not
wrong, it was the wrong *kind* of code for that endpoint.

So the loader also builds a **station map** beside the alias map: station code ->
its platforms' codes, sorted. Only a code that is itself a `location_type = 1`
row is a key, and a platform that shares its station's code is not listed under
it.

**The rule, per endpoint.** This is what every stop-keyed endpoint does with each
kind of code, on a DB feed that has stations. "unchanged" means byte-identical to
before this section existed.

| endpoint | platform or plain code | station code, before | station code, now | unknown code |
| --- | --- | --- | --- | --- |
| `GET /stop/{g}/{c}` | the stop | the **station row** | unchanged - the station row | 404 |
| `POST /getAllStopsByIds` | the stops | the station row | unchanged - the station row | omitted |
| `GET /route-stop-mapping/{g}/stop/{c}` | its own rows | the platforms' rows, in `HashSet` order | the same rows, in sorted platform order, `X-Stop-Expanded` | 404 |
| `GET …/stop/{c}?direction=` | as above | as above | as above | 404 |
| `GET …/stop/{c}?allowClusters=true` | one row per route over the stop's cluster | **fell through to the plain fan-out**: no cluster widening, no per-route dedup | one row per route over the union of the platforms' clusters, `X-Stop-Expanded` | 404 |
| `POST /getAllRouteStopMappingsByStopCodes` | its rows | the platforms' rows | the same, sorted; a station and its own platform in one request still de-duplicate | omitted |
| `GET /cluster/{g}/destinations/{c}` | destinations downstream of its cluster | **`[]`** | the union over the platforms' clusters, deduplicated per destination cluster, `X-Stop-Expanded` | `[]` |
| `GET /cluster/{g}/routes/{from}/{to}` | direct routes between the two clusters | **`[]`** | either end widens from its platforms' clusters, `X-Stop-Expanded` | `[]` |
| `GET /station-children/{g}/{c}` | `[]` | its platforms, in `HashSet` order | its platforms, **sorted** | `[]` |
| `GET /alternateStops/{g}/{c}` | stops with the same normalised name | same-name stops | unchanged - deliberately | `[]` |
| `GET /stop-code/{g}/{provider}` | the mapped code | n/a | unchanged | 404 |

Each row a station answers with keeps **its own platform's `stopCode`** - the
code a bus calls at, which is what an ETA or a GTFS-RT vehicle join is keyed on.
No response gains a field and no field is renamed. Where a station was expanded
the response carries `X-Stop-Expanded: <station>=<n platforms>`, in the style of
`X-Stop-Alias`, and the expansion is logged at info at most once an hour per
station.

**Left alone, on purpose:**

- `/stop` and `getAllStopsByIds` **must** keep returning the station row.
  Substituting a platform would silently change what a saved stop or a deep link
  means, and a caller asking for a station is asking about the place.
- `/alternateStops` groups by normalised stop *name*, not by geometry or
  parentage. A station and its platforms share a name, so a station code already
  lists the platforms and a platform already lists its station; nothing about
  stations makes that answer better or worse, and widening it would change what a
  plain code returns too.
- A **station with no live platforms** is not expanded. Its route-stop mapping
  still 404s rather than becoming an empty `200`: "nothing is under this station"
  is not "this station has no service", and turning the first into the second is
  the failure this section exists to remove.
- `/stop-code/{g}/{provider}` maps a provider's own code to a GIMS one. Provider
  mappings name the stops a provider knows, never a station GIMS invented.

**Composing with the merge aliases.** A code is resolved through the alias map
first and the station map second, and only that order works: an alias always
names a *live* stop, and only a live stop can be a station. So a retired station
code expands to the **survivor's** platforms, and a platform merged away under a
station answers as the platform that survived it - not as the whole station,
which would widen the answer behind the caller's back. A station whose platform
was merged away lists only what is live, on the same poll that brings the merge
in. (The editor refuses `stop/merge` on a station - `stop_is_station`, "only
stops are merged". A retired *station* code comes from `station/merge`, section
5's "Merging duplicate stations", which writes the same `merged_into` provenance;
the alias map is built from those rows whatever wrote them.)

**A preprocessed feed has no station map at all.** Its stops carry `stationId`
(metro platforms point at their station) but no row is `location_type = 1`, so
nothing is a key and no read on it can expand. Reverting a feed out of DB mode
drops its entry, exactly as it drops the alias map. The one visible change on a
preprocessed feed is that `/station-children` is now **sorted**: it was answered
straight out of a `HashSet`, and Rust seeds its hasher per process, so two pods -
or the same pod after a restart - already returned the same codes in a different
order. Verified by running `origin/main` twice against the same data: the two
processes disagreed on the order for every station asked for.

Cost, measured on a local copy of `chennai_bus` with the station layer applied
(10,113 served stops, of which 2,258 are station rows standing over 5,433
platforms), against the same binary without the map: the per-feed rebuild the
version poll runs is **10,716 ms median without the map and 10,784 ms with it**
(4 reloads each, 10,407-11,338 against 10,455-11,384) - the ranges overlap and
the difference is inside the noise of a rebuild that size. The map itself adds
**264 KB** to the served data (`children_bytes` 355,173 -> 625,435, of ~42 MB
accounted). Boot is likewise unmoved: the same feed's DB load measured
1,669-2,019 ms either way over three boots each. `chennai_bus` as it stands has
no station rows, so its map is empty and none of its numbers move at all. The map
is built once per feed load and rebuilt on the version poll, like the alias map;
a request costs one hash lookup.

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
  submitted, neither approve nor commit** · `admin` everything plus users, the
  feed's data source (a `feed_config` change, section 3), and the override below.
- **Admin self-approval (the one exception to maker-checker).** An admin may
  approve a set they submitted by saying so: `POST /change-sets/{id}/approve`
  with `{self_approve: true}`. Without the flag the admin gets the same 403
  `own_change_set` as anyone, so it never happens by accident;
  `details.can_self_approve` tells the dashboard whether the override is open to
  the caller. The set is marked `self_approved`, the same admin may then commit
  it (the override covers approve **and** commit), and the audit log records it
  as `change_set_self_approved` — never as `change_set_approved` — with the
  later `change_set_committed` carrying `self_approved: true`. The dashboard
  shows a "self-approved" badge on the draft and its list row, and the history
  row stands out. Nobody but an admin has it, and there is no such override for
  reject.
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
account · 404 · 409 conflict or wrong status · 429 locked · 503 `try_again`
(a transaction lost a lock-order race with another change to the feed three
times running; nothing was applied — send the same request again; see "Commit"
below). Every list is
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
| 503 | `try_again` | any editor write or draft replay: the transaction hit a deadlock or serialization failure on every one of its 3 retries; nothing was applied — repeat the request (section 3, "Commit") |

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
| GET | `/feeds/{g}/stops?q=&bbox=minLat,minLon,maxLat,maxLon&station=&limit=&cursor=` | `q` trigram on name, exact on id/code. `station=true` lists stations only; `station=<id>` lists that station's platforms. Item: the stop row (`stop_id, stop_code, name, lat, lon, location_type, parent_station, platform_code, description, cluster_id, regional_name, hindi_name, info_json, position_source, provenance, deleted, row_version, updated_at, updated_by`) + `route_count` + `platform_count` (a station's live platforms; 0 for a stop — section 11) |
| GET | `/feeds/{g}/stops/{stop_id}` | the stop row + `route_count` + `platform_count` + `routes: [{route_id, short_name, long_name, sequence, stop_type, stage_no}]` + `children` (a station's platforms, each with `route_count`) + `nearby` (≤ 60 m, nearest first, each with `distance_m` and `route_count`) + `parent` (the station row, or null) |
| GET | `/feeds/{g}/routes?q=&limit=&cursor=` | route row without the polyline + `has_polyline`, `stop_count` (served rows) |
| GET | `/feeds/{g}/routes/{route_id}` | route row (`route_id, short_name, long_name, route_type, agency_id, color, text_color, encoded_polyline, polyline_source, service_type, provenance, deleted, row_version, …`) + `stop_count` + `rows_hash` + `rows: [{sequence, stop_id, stop_name, lat, lon, stop_deleted, parent_station, stop_type, stage_no, stage_name, marker_id, marker_name, marker_lat, marker_lon, stop_name_override, provider_id}]`. `stop_name` is the route's own spelling when it has one (`stop_name_override`), else the stop's name |
| POST | `/feeds/{g}/routes/{route_id}/polyline:osrm?change_set=` | `{route_id, encoded_polyline, polyline_source: "osrm", waypoints, distance_m, saved: false}` — a proposal through the served stops and markers (of the draft, with `change_set`); not saved, add it as a `route` change. Asked of OSRM in chunks of at most 25 waypoints; a failure is 502 `osrm_failed` saying why and where (section 17.5) |
| POST | `/feeds/{g}/routes/{route_id}/polyline:gps?change_set=` | `{route_id, encoded_polyline, polyline_source: "gps", saved: false, evidence}` — the path this route's buses drove over the last 14 days, snapped to roads (section 17); not saved, add it as a `route` change |
| GET | `/feeds/{g}/stops/{stop_id}/context`, `/feeds/{g}/routes/{route_id}/context` | what is known about a stop or a route for cleanup: detours, coordinate reviews, same-named stops, its audit rows, the open drafts touching it — section 9 |
| GET | `/feeds/{g}/audit?change_set=&limit=&cursor=` | newest first: `{audit_id, at, actor, actor_email, action, gtfs_id, change_set_id, detail}` |

Audit actions: `seed`, `release`, `user_bootstrapped`, `user_created`,
`user_updated`, `user_totp_reset`, `totp_enroll_started`, `totp_confirmed`,
`totp_failed`, `session_created`, `change_set_created`, `change_added`,
`change_updated`, `change_removed`, `change_set_submitted`, `change_set_approved`,
`change_set_self_approved` (an admin approved a set they submitted, section 2:
`{submitted_by, submitted_by_email, comment}`), `change_set_rejected`,
`change_set_reopened`, `change_set_discarded`, `change_set_committed` (detail
`{feed_version, changes, applied, self_approved}`), and from
sections 5 and 6: `bulk_imported` (`{kind, rows, changes, first_change_id,
last_change_id}`), `stop_merged` (one per merge at commit: `{change_id, from, into,
routes, rows, keep_name, keep_position}`), `station_merged` (one per station merge
at commit: `{change_id, from, into, platforms_moved, keep_name, keep_position}`),
`station_proposal_approved`,
`station_proposal_rejected`, `station_proposal_reopened`,
`station_proposal_returned` (`detail.reason` is `change_removed` or
`change_set_discarded`) and `station_proposal_committed`. Approving an area
writes one `station_proposal_approved` per station (`detail.bulk = true`). nandi's
scripts write `seed`, `release` and `station_proposals_built` (`{batch, proposals,
platforms, diameter_m, base_version}`). Coordinate reviews (section 8) write
`position_review_moved`, `position_review_split`, `position_review_merged`
(section 8.2), `position_review_confirmed`, `position_review_reopened`,
`position_review_returned` and `position_review_committed`, nandi's loader
`position_reviews_loaded`, and nandi's `editor/autofix_same_name.py`
`position_reviews_autofix_planned` (`{tool, reviews, fits_m, off_route_m, merge,
move, choose, none}`) when it stores its candidates on the reviews (section 8.2).
Committing a `feed_config` change ("Feed data source"
below) writes `feed_data_source_changed`, detail `{gtfs_id, from, to, change_id,
change_set_id}`. Saving the webhook policy (section 12.5) writes
`webhook_settings_updated`, detail `{from: {enabled, allowed_hosts, source}, to:
{enabled, allowed_hosts}}`, with no `gtfs_id`: the policy is the deployment's,
not a feed's. The dashboard's history has words for every one of these
(`ACTION_LABEL` in `editor-ui/js/admin.js`; `dev/ui_e2e.mjs` checks each action in
the database has a label).

### Feed data source

Whether GIMS serves a feed from these tables or from its preprocessed data —
section 1's `gtfs_feed.data_source`. **Nothing writes it directly.** It is
changed like everything else in this document: a `feed_config` / `update` change
in a draft (the change table below), submitted, approved by someone else and
committed. Every GIMS pod picks the committed value up on its next
`gtfs_version_poll_seconds` poll (default 5 s) — no restart.

| method | path | body → response |
|---|---|---|
| GET | `/feeds/{g}/config` | viewer+ → `{gtfs_id, data_source: "db"\|"preprocessed", version, pending: [{change_set_id, change_set_title, status, change_id, data_source}]}` — `data_source` is the raw `gtfs_feed.data_source` value, the vocabulary of `/feeds`; `pending` lists the `feed_config` changes of the feed's open change sets (`draft`, `submitted`, `approved`), latest-edited set first. 404 `feed_not_found` if `g` has no `gtfs_feed` row at all |

`POST /feeds/{g}/config`, which used to switch the data source at once, is
**removed** (404 `endpoint_not_found`, as for any unknown path). The dashboard's
Feed settings page adds the change through `POST /change-sets/{id}/changes`.

Settled while implementing:

- The change: `{entity: "feed_config", op: "update", entity_key: <gtfs_id>,
  after: {data_source}}`. **Admin only** to add or edit (403 `role_required`
  otherwise, whoever's draft it is); anyone who may edit the draft may remove it.
  Submitting, approving and committing a set that carries one follow the normal
  rules — editor+ submits, approver+ approves and commits, never the submitter
  (unless section 2's admin override is used). The admin's say is in drafting it.
- Refused when added or edited, 400 `invalid_change`: a value other than `db` /
  `preprocessed`, or any other field in `after` (`details.code =
  "invalid_data_source"`); an `entity_key` that is not the change set's feed
  (`details.code = "feed_mismatch"`); any `op` but `update`.
- `before` is the read shape `{gtfs_id, data_source, version}` at the time the
  change was added. **`base_row_version` does not carry the base** (it stays
  `null`): the only version a feed has moves with every commit, so a change based
  on it would conflict with every unrelated commit. The base is
  `before.data_source`: if the live value at submit or commit differs, that is 409
  `change_set_conflicts` with the usual conflict entry (`entity: "feed_config"`,
  `entity_key` the feed, `reason: "changed"`, `expected` / `actual` the two
  values, "The data source of feed … was changed by another commit …") — also when
  the live value already is what the change wants.
- A change to the value the feed already has is the validation **warning**
  `data_source_unchanged`; it does not block submit, and at commit it switches and
  audits nothing. Earlier changes of the same draft count as applied, so of two
  switches to `db` in one draft the second is the warning.
- Commit applies it inside the commit transaction (`UPDATE gtfs_feed SET
  data_source`); the commit's own `version = version + 1` is the only bump.
  `feed_data_source_changed` is written at commit (actor: whoever commits), one
  row per change that switched something; drafting it writes the usual
  `change_added`.
- A draft's route preview and every other draft read ignore the change; `GET
  /change-sets/{id}` shows it with its `before` / `after` like any other.
- A change set needs a `gtfs_feed` row to exist (`POST /feeds/{g}/change-sets` is
  404 `feed_not_found` without one), so the old "first switch creates the row" is
  gone: a feed's row is created by nandi's seed, never by the dashboard.

### Drafts

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/change-sets?status=` | `status` may be a comma list (`draft,submitted,approved`) |
| POST | `/feeds/{g}/change-sets` | `{title, description?}` → 201 draft, `base_version` = current feed version |
| GET | `/change-sets/{id}` | the set (`change_set_id, gtfs_id, title, description, status, created_by(_email), submitted_by(_email), reviewed_by(_email), committed_by(_email), *_at, review_comment, self_approved, base_version, committed_version, change_count` — the list items carry the same fields) + `changes` + `validation: [{change_id, level: error\|warning, code, message}]` + `conflicts: [{change_id, entity, entity_key, reason, expected, actual, message}]` + `can_submit` + `stop_names` (`{stop_id: name}` for every stop a `route_stops` change references) |
| POST | `/change-sets/{id}/changes` | append one change (below); draft only; editor+ → 201 the set, plus `change_id` |
| PUT | `/change-sets/{id}/changes/{change_id}` | `{after, base_row_version?}` replaces a change's `after` → the set |
| DELETE | `/change-sets/{id}/changes/{change_id}` | → the set |
| GET | `/change-sets/{id}/preview/routes/{route_id}` | the route detail (same shape as above) with the draft applied, plus `validation` and `conflicts` |
| POST | `/change-sets/{id}/submit` | draft → submitted; 400 `validation_failed` with errors, 409 `change_set_conflicts` |
| POST | `/change-sets/{id}/reopen` | submitted/rejected/approved → draft; its creator, its submitter, or an admin. Clears `self_approved` |
| POST | `/change-sets/{id}/approve` | `{comment?, self_approve?: bool}` submitted → approved; approver+, 403 `own_change_set` for the submitter (`details.can_self_approve`) — **unless** the submitter is an admin and sends `self_approve: true` (section 2), which approves it, marks it `self_approved` and audits `change_set_self_approved` |
| POST | `/change-sets/{id}/reject` | `{comment}` (required) submitted → rejected; approver+, 403 `own_change_set` for the submitter, no override |
| POST | `/change-sets/{id}/commit` | approved → committed; approver+, 403 `own_change_set` for the submitter — unless the set is `self_approved` and they are (still) an admin; see below |
| POST | `/change-sets/{id}/discard` | not committed/discarded → discarded; its creator or an admin |

Editing a set that is not a draft is 409 `change_set_not_draft`.

Settled while implementing (self-approval):

- `self_approve: true` on **someone else's** set is an ordinary approval: the flag
  is ignored, the set is not marked, the audit row is `change_set_approved`.
- A non-admin sending `self_approve: true` on their own set is 403
  `own_change_set` with `details.can_self_approve: false`; an admin without the
  flag (or with `false`) the same with `true`. Reject and commit refusals carry
  `can_self_approve: false`: the override is asked for on approve only.
- The body of approve is optional, and a body that does not parse counts as none —
  so a malformed `self_approve` (`"yes"`) is never the override: 403.
- Commit of an own set that someone **else** approved stays 403 for its submitter,
  admin or not: the override is the explicit approval, not the role. An admin
  demoted after self-approving can no longer commit it.
- `self_approved` is cleared by reopen and by submit, and stays on the set once
  committed. The table enforces it (`0010`): the submitter may be the reviewer
  only on a `self_approved` set, and only such a set may be so marked.

A change is `{entity, op, entity_key, after, base_row_version?}`. Its `before` is
a snapshot in read shape: the stop or route row; for a station, the row plus
`member_stop_ids`; for `route_stops`, the route detail's `rows` list. A merge's
`before` is both sides and what moves - `{from, into, affected}` for a stop,
`{from, into, moving_platforms}` for a station (section 5).

| entity / op | `after` | validation |
|---|---|---|
| `stop` / `update` | any of `name, lat, lon, platform_code, description, cluster_id, regional_name, hindi_name` | lat/lon together and in range; a move > 500 m is a **warning**; `platform_code` ≤ 120 and `description` ≤ 500 characters (section 11) |
| `stop` / `create` | `{stop_id, name, lat, lon, stop_code?, platform_code?, description?, cluster_id?, regional_name?, hindi_name?}` | `entity_key` = `stop_id`; id unused; ids are 1–64 of `A–Z a–z 0–9 _ - .` (no `:` — GIMS splits ids on it) |
| `stop` / `delete` | `null` | error if any route row still uses it |
| `route` / `update` | any of `short_name, long_name, color, text_color, encoded_polyline, polyline_source` | colour `#RRGGBB`; polyline decodes to ≥ 2 points; `polyline_source` is `osrm`, `gps`, `manual` or `imported` |
| `route_stops` / `replace` | `{rows: [...], base_rows_hash}` — the whole ordered list | see below |
| `station` / `create` | `{station_id, name, lat, lon, description?, member_stop_ids: [...]}` | `entity_key` = `station_id`; at least two members (`too_few_members`, refused when added: 400 `invalid_change`); members exist, are location_type 0, have no other parent |
| `station` / `update` | `{name?, lat?, lon?, description?, member_stop_ids?}` | same; members, when sent, are at least two (to ungroup a station, delete it); a station has no `platform_code` of its own |
| `station` / `delete` | `null` | clears members' `parent_station` |
| `feed_config` / `update` | `{data_source: "db"\|"preprocessed"}` | **admin only**; `entity_key` = the change set's feed; see "Feed data source" above |

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

**Commit** — one transaction: the feed's advisory lock (below), then `SELECT …
FROM gtfs_feed WHERE gtfs_id = $g FOR UPDATE`; for each change in `position`
order check the target's current `row_version` (stop, route), `rows_hash`
(route_stops) or `data_source` (feed_config) against the change's base — any
mismatch aborts with 409 `change_set_conflicts` and `details.conflicts`; apply;
re-run validation on the result; `version = version + 1`; set
`committed_version`; audit. Nothing is applied if anything fails.

**Every replay of a feed's drafts is serialised on one advisory transaction
lock** — `SELECT pg_advisory_xact_lock(hashtext('gtfs_editor:' || $g))`, taken
first thing by every transaction that replays or writes a draft of feed `g`: the
commit, submit's validation, `GET /change-sets/{id}` and the route preview
(both replay the draft with real UPDATEs that are rolled back), add / PUT /
DELETE change, bulk import (dry run included), a coordinate review's move, split
and merge and its what-if merge. A commit and a concurrent add therefore queue,
one after the other, instead of taking the same live rows in different orders
and deadlocking. Reads that do not replay (stops, routes, lists, audit, context)
never take it.

Settled while implementing (the feed lock, 2026-09-18):

- Why: while a script was adding ~560 `route_stops/replace` changes to one
  draft — each add's response replays the whole draft — a person committed
  another draft of 13 stop moves and merges on the same feed. Postgres killed
  two of the commit's statements with `deadlock detected` (SQLSTATE 40P01), and
  the commit answered 400 `validation_failed` with the per-change finding
  `{"code": "database_rejected", "message": "deadlock detected"}` — a transient
  lock-order collision reported as if the changes were wrong. Retrying later
  worked.
- The lock is `src/editor/feed_lock.rs`'s `lock_feed` (by feed) and
  `lock_feed_of_set` (by change set: reads the set's feed without a lock, then
  locks the feed — so a transaction never holds a row while it waits for the
  feed). It is taken before the first row lock of the transaction; the commit's
  order is feed lock, feed row, set row. Held only to the end of the
  transaction; nothing to release, nothing to leak.
- A deadlock (40P01) or serialization failure (40001) anywhere in one of these
  transactions is **never a finding**: `evaluate` propagates it out of the
  change's savepoint instead of writing `database_rejected`, and every
  `sqlx::Error` with either code becomes 503 `try_again`. The transaction is
  retried whole up to 3 times, after 50 / 150 / 400 ms, before that 503 reaches
  the caller. Each retried unit is exactly one transaction (a commit is one
  transaction; an add's response is a second, retried on its own), so a retry
  never applies anything twice. `database_rejected` stays what it was for a
  genuine refusal (a constraint, a bad value).
- Cost, measured locally: the lock statement itself 0.3 ms and the set's feed
  lookup 0.4 ms (psql `\timing`); a 5,000-row `stop_updates` dry run 274–283 ms
  before and 290–304 ms after (run to run noise on the same machine is of that
  order); one add to a draft of 16 stop-list changes 50 ms before, 48–51 ms
  after. Under a concurrent commit an add averaged 93 ms before (1.09 s worst,
  the deadlock timeout) and 67 ms after (113 ms worst); the commit itself 439 ms
  before (1.08 s worst) and 46 ms after. `tests/editor_feed_lock_flow.rs` runs
  30 commits against 300 concurrent adds and removals and found 12 deadlock
  findings before the lock, none after.
- The dashboard treats 503 `try_again` like any other error: it shows the
  message ("another change to this feed was being applied at the same time;
  please try again").

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

Delivery: "Where GIMS may send" — the webhook policy of section 12.5, with the
on/off switch, the allow-list and a line saying whether it is coming from the
database or from this deployment's configuration. An admin changes it there;
everyone else sees the same block, read-only. Feed settings (admin): each feed's
data source, the drafts already carrying a switch of it (from `GET /feeds/{g}/config`'s `pending`, linked), and "Add to
draft: switch to …", which adds a `feed_config` change to the current draft —
for the feed chosen in the top bar, since a draft belongs to one feed — after a
confirm that says it takes effect only once the draft is submitted, approved by
someone else and committed. Drafts: an admin looking at a draft they submitted
gets "Approve it myself (admin override)" beside the disabled Approve, behind the
confirm "You submitted this draft. Approving it yourself skips the second
reviewer. Continue?"; a self-approved draft carries a "self-approved" badge in its
header and list row, and its history row is highlighted with an "admin override"
badge.

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
| `stop` / `create` | `{stop_id?, name, lat, lon, stop_code?, platform_code?, description?, regional_name?, hindi_name?}` | as above; **`stop_id` may be omitted**: the server mints `ed_` + 10 lower-case hex (unused) and returns it in the change (`entity_key` and `after.stop_id`) |
| `route` / `create` | `{route_id, short_name, long_name?, route_type?, color?, agency_id?}` | `entity_key` = `route_id`; id unused (409-style row error `route_exists`); same id charset as stops; `route_type` defaults to 3, `agency_id` to the feed's usual one; `short_name` required |
| `route` / `delete` | `null` | soft delete (`deleted = true`); refused while another pending change in the set edits the route |
| `station` / `create`, `update` | may carry `members: [{stop_id, platform_code?}]` instead of `member_stop_ids` | as before; `platform_code` ≤ 120 chars; a station change that came from a proposal carries `proposal_id` |
| `stop` / `merge` | `{into_stop_id, into_row_version, keep_name?: "into"\|"from", keep_position?: "into"\|"from", position_review_id?}` | see *Merging duplicate stops* below; `position_review_id` is stored and ignored, as on `stop/update` (section 8.2) |
| `station` / `merge` | `{into_station_id, into_row_version, keep_name?: "into"\|"from", keep_position?: "into"\|"from"}` | see *Merging duplicate stations* below; `entity_key` = the station that goes away. Both ids are live stations (`not_a_station`, `station_not_found`, `station_deleted`), different (`merge_same_station`), and the one that stays has no parent (`into_station_has_parent`). No route row moves |

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
- The merged-away id does **not** stop answering: step 4's `merged_into`
  provenance is what the GIMS loader turns into a stop alias, so every public
  read for the old id answers with the stop that survived - section 1,
  "Merged-away stop ids keep answering".

### Merging duplicate stations (2026-09-19)

`station` / `merge` merges one station into another: every platform of the
station that goes away becomes a platform of the one that stays, keeping its own
`platform_code`, and the station that goes is soft-deleted. It is a **separate
change type**, not a loosening of `stop` / `merge`, which still refuses a station
on either side (`stop_is_station`, "only stops are merged") exactly as it always
has. Nothing about merging two stops changes.

**Route rows are never touched.** `gtfs_route_stop` only ever names platforms
(section 1), a platform's id does not change here, and so no route, no fare stage
and no stop list moves. That is why this op has no `merge_would_repeat_stop`, no
`merge_same_route_twice` and no `affected` routes: there is nothing to repeat.

- `entity_key` = the station that goes away (the "from" station);
  `base_row_version` = its `row_version`; `after` is
  `{into_station_id, into_row_version, keep_name?: "into"|"from",
  keep_position?: "into"|"from"}`. `into_row_version` is filled from the live row
  when left out, as on `stop` / `merge`; `keep_name` and `keep_position` default
  to `"into"`. No other key is accepted (a station merge carries no
  `position_review_id`: a coordinate review is about one stop's point).
- `before` (read shape) = `{from: <station row>, into: <station row>,
  moving_platforms: [{stop_id, name, platform_code, route_count}]}` - the live
  platforms that would change parent, in id order, each with the number of live
  routes through it. A platform an earlier merge already retired is not live and
  is not listed.
- **Errors**: both ids exist (`station_not_found`), are not deleted
  (`station_deleted`) and are `location_type = 1` (`not_a_station`, whose message
  and key name **which** id is the stop); they are different ids
  (`merge_same_station`, a shape check when the change is added); and the station
  that stays has no `parent_station` of its own (`into_station_has_parent`) -
  otherwise the platforms would land on a station that is itself a platform.
  `station_merged_away` on any later change in the same draft that uses the from
  station (a station update, delete or merge).
- **Warnings**: `station_merge_far_apart` (the two station points more than
  **500 m** apart; the message says the distance), `station_merge_names_differ`,
  `station_merge_no_platforms` (the from station has no live platforms, so the
  merge is really just a delete) and `station_merge_pending_proposal` (a
  `gtfs_station_proposal` still `pending` names either station as its
  `station_id` or lists it among its members; the message names the proposals).
  As everywhere, a warning never blocks submit or commit.
- **Apply** (preview replay and commit, the same code path):
  1. `UPDATE gtfs_stop SET parent_station = into WHERE parent_station = from AND
     NOT deleted`. Each platform keeps its own `platform_code`: the label says
     which way the buses there go, which the merge does not change.
  2. `keep_name` / `keep_position` `"from"` copy the from station's name /
     lat+lon onto the kept station.
  3. The from station: `deleted = true`, `parent_station = NULL`, provenance
     `{"merged_into": into}`.
- **Commit** checks both `row_version`s; a mismatch is 409 `change_set_conflicts`
  in the usual conflict shape, `entity` `station` (the kept station's conflict has
  its own id as `entity_key` and says "kept by the merge of &lt;from&gt;"). Commit
  audits `station_merged` with `{from, into, platforms_moved, keep_name,
  keep_position}`.
- **It composes with section 1, in one order: alias first, expansion second.**
  Step 3 writes the *same* `merged_into` provenance key a stop merge writes, so
  the loader's alias map picks the retired station code up unchanged, and what it
  resolves to is a live station, which the station map then expands. So
  `GET /stop/{g}/<old station>` answers with the surviving **station row**, and
  `GET /route-stop-mapping/{g}/stop/<old station>` answers with the **survivor's**
  platforms (`X-Stop-Alias` and `X-Stop-Expanded` both set). A platform merged
  away *before* its station was merged still resolves to the platform that
  survived it, not to the whole station. `tests/editor_station_merge_flow.rs`
  asserts all three against a real feed load.

Settled while implementing:

- **Why 500 m for `station_merge_far_apart`, and not the 150 m a stop merge
  uses.** The stop threshold is about two kerbs, which are the same piece of
  pavement or they are not. A station point is the centroid of its platforms, and
  `build_stations` (section 6) already groups same-named stops within a 500 m
  diameter, so two stations that are really one place have their points inside
  that same 500 m; a wider gap is a grouping the builder deliberately did not
  make. Measured on the local `chennai_bus` seed's 2,258 station proposals
  (`spread_m` median 32 m, p90 254 m, p99 475 m): of the 95 pairs of proposals
  that share a name, **0 are within 250 m** of each other point to point, **10
  are within 500 m**, and the median gap is 1,018 m (max 52 km - the same name in
  two parts of the city). So 500 m stays quiet for exactly the ten plausible
  merges and warns on the other 85. 150 m or 250 m would have warned on all 95,
  which is a warning nobody reads.
- The change type is `station` / `merge` with its own `after` field
  (`into_station_id`, not `into_stop_id`) so that nothing which dispatches on
  `into_stop_id` - the draft view, the pending overlay, the coordinate reviews -
  silently treats a station merge as a stop merge.
- A station a draft merges away is gone for that draft's later changes, exactly
  as a merged stop is: the finding is `station_merged_away`, the station version
  of `stop_merged_away`, and it names the station that survived.
- A **deleted** platform whose `parent_station` was the from station keeps
  pointing at it: the from station's row stays (soft delete), so the reference is
  still valid, and only live platforms move. A station's `before` and
  `station-children` both list live platforms only, so nothing downstream sees it.
- The dashboard's station page offers "Merge into another station…" beside "Edit
  station" and "Dissolve station"; a plain stop is offered "Merge with a
  duplicate…" as before, and neither is offered the other. The nearby list on a
  station offers "Merge…" against a nearby **station**, as the one on a stop does
  against a nearby stop. `editor-ui/js/station_merge.js` is the screen; the stop
  picker takes `kind: "station"` to search and click stations only.

### Bulk import — preview, then add to a draft

`POST /change-sets/{id}/bulk` (editor+, draft only), body
`{kind, rows, dry_run}` with at most 5,000 rows:

| `kind` | row | becomes |
|---|---|---|
| `stops` | `{stop_id?, name, lat, lon, platform_code?}` | one `stop/create` per row |
| `routes` | `{route_id, short_name, long_name?, color?}` | one `route/create` per row |
| `route_stops` | `{route_id, sequence, stop_id, stop_type, stage_no, stage_name}` | one `route_stops/replace` per route (rows sorted by `sequence`) |
| `stop_updates` | `{stop_id, platform_code?, description?, name?}` | one `stop/update` per row (a station: `station/update`), with exactly the cells given — section 11 |

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
**merges** it into a stop of the same name that stands where its routes pass
(section 8.2; a merge is a review's only action), or **confirms** the position is
right, which closes it with no change. A stop can need
more than one of these: a busy stop can carry routes from several original places,
so a review may collect several moves and splits — never more than one move — in
one draft before it is released through the normal submit / approve (someone else)
/ commit.

| method | path | body / rule |
|---|---|---|
| GET | `/feeds/{g}/position-reviews?status=&bbox=&q=&auto_fix=&limit=&cursor=` | `auto_fix` = `merge`\|`move`\|`choose`\|`none` filters on `evidence.auto_fix.action` (section 8.2). `status` comma list (default `pending`); `bbox` on the loaded position; `q` trigram on name, exact on stop id. Item: `{review_id, stop_id, original_stop_id, stop_name, reason, lat, lon, raw_lat, raw_lon, suggested_lat, suggested_lon, suggested_source, evidence, status, change_set_id, change_set_title, reviewed_by_email, reviewed_at, review_note, batch}` |
| GET | `/feeds/{g}/position-reviews/summary` | `{pending, approved, committed, confirmed, auto_fix: {merge, move, choose, none}}` — `auto_fix` counts the **pending** reviews by `evidence.auto_fix.action` |
| GET | `/position-reviews/{id}` | the item plus, computed now from the tables: `stop` (current row), `routes: [{route_id, short_name, sequence, prev: {stop_id, name, lat, lon} \| null, next: {...} \| null}]` (every route that calls at the stop, previous and next SERVED stop), `detour_m` (median over routes of d(prev,stop)+d(stop,next)−d(prev,next)), and `problems: [{code, message}]` (`stop_deleted`, `stop_merged_away`, `moved_since_load` when the stop is > 25 m from the loaded position) |
| POST | `/position-reviews/{id}/move` | editor+. `{change_set_id, lat, lon, note?}` → appends `stop/update` `{lat, lon, position_review_id}` (base_row_version = the stop's current) to the draft; review → `approved` (or stays `approved`, gaining this action, if it already has splits in the same draft). The response is the review detail, with `detour_m_after` for the new point |
| POST | `/position-reviews/{id}/merge` | editor+. `{change_set_id, into_stop_id, keep_name?, note?}` → appends one `stop/merge` of the reviewed stop into a same-named stop; section 8.2 |
| POST | `/position-reviews/{id}/confirm` | editor+. `{note?}` pending → `confirmed` |
| POST | `/position-reviews/{id}/reopen` | editor+. confirmed → pending |

`stop/update` accepts an optional `position_review_id`. Lifecycle, as for station
proposals, but a review's draft can hold several of its changes at once (section
8.1): removing one leaves the rest and the review `approved`; removing the last one,
or discarding the draft, returns the review to `pending`; committing marks it
`committed`. Every action is audited (`position_review_moved`,
`position_review_split`, `position_review_merged`, `position_review_confirmed`,
`position_review_reopened`,
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

### 8.2 Merging a reviewed stop into a same-named stop

**Why.** Many suspects are duplicates: another stop of the same name already
stands where the suspect's routes pass. nandi's advisory tool looks, for each
pending review, for other stops of the same name and tests whether pointing the
suspect's calls at the candidate removes the detour. It stores what it found in
the review's `evidence`, and drafts the clear cases through this API; nothing is
live until the draft goes through submit / approve (someone else) / commit.

**Evidence** (the tool writes it; the API returns `evidence` as stored and does
nothing else with it, beyond the list filter and summary counts of section 8):

- `evidence.same_name_candidates: [{stop_id, name, lat, lon, distance_m,
  name_similarity, route_count, detour_m_after, shares_route: bool, verdict:
  "fits"|"no_fit"}]`
- `evidence.auto_fix: {action: "merge"|"move"|"choose"|"none", into_stop_id?,
  lat?, lon?, detour_m, detour_m_after?, reason, tool, threshold_m}`
- As nandi's `editor/autofix_same_name.py` writes them (2026-09-17): a candidate
  also has `same_words` (the two names are the same words once case, initials and
  abbreviations are read away - only such a candidate is ever picked; one name
  inside the other is listed only) and `runs_opposite` (its buses head the other
  way: the facing kerb, never a fit); on a review with `mixed_origins` each
  candidate names the route group it fits, `fits_route_ids` and
  `fits_origin_stop_id`, and the action is `choose` - those routes need a split,
  not a move of the whole stop. `auto_fix` also has `off_route_m`: a review whose
  stop is closer to its routes than that is `none`, whatever namesakes it has.
  `move` means "same place, but the id must survive" (a station platform, or the
  survivor of an earlier merge): `lat`/`lon` are the candidate's point.

**Endpoint.** `POST /position-reviews/{id}/merge`, editor+. Body
`{change_set_id, into_stop_id, keep_name?: "into"|"from", note?}`. It appends ONE
`stop/merge` to the draft: `entity_key` the reviewed stop, `base_row_version` its
current version, `after` = `{into_stop_id, into_row_version (live), keep_name
(default "into"), keep_position: "into", position_review_id}`. Review →
`approved` with `change_set_id` / `change_id`. Response: the review detail, plus
`warnings: [{level, code, message}]`.

**Rules.**

- The change is added by the code path `POST /change-sets/{id}/changes` uses
  (`service::add_change_to`), and judged by the merge's own validation
  (`service::findings_for` runs the draft plus this merge through `evaluate`, in a
  savepoint that is rolled back, before anything is stored). An **error** there —
  `merge_would_repeat_stop`, `merge_prm_stop`, `merge_same_stop`,
  `stop_is_station`, `stop_not_found`, `stop_deleted`, `stop_merged_away` —
  refuses the call: 400 `review_has_problems`, `details.problems: [{level, code,
  message}]` (the warnings are listed there too). **Warnings** —
  `merge_far_apart`, `merge_names_differ`, `merge_same_route_twice` — do not, and
  come back as `warnings`.
- Status goes the way `/move` and `/split` go, by the same code
  (`may_add_action`, 409 `review_in_other_draft`, 409 `review_not_pending`,
  pending again when the change is removed or the draft discarded, `committed` on
  commit).
- 409 `draft_conflict` (`details.change_ids`) when the draft already has **any
  change of this review** — a merge takes the stop away, so it is a review's only
  action — or a change to the reviewed stop (a `stop` change keyed on it, or a
  `stop/merge` into it), or a `stop` update, delete or merge **of the stop it
  would merge into**. A later `/move`, `/split` or second `/merge` on a review
  that has a merge in the draft is 409 `draft_conflict` naming that merge.
- Audit `position_review_merged`, detail `{review_id, stop_id, into_stop_id,
  change_id, change_set_id, detour_m, detour_m_after, moved_m, note}`:
  `detour_m_after` is the reviewed stop's calls measured where the into stop is,
  `moved_m` the distance between the two stops.
- A draft edit (PUT) of the change keeps its `position_review_id`, by the rule a
  move's edit follows (400 `invalid_change`, `position_review_mismatch`).

**Detail.** `draft_actions` gains `{kind: "merge", change_id, into_stop_id, lat,
lon, detour_m_after}` (`lat`/`lon` the into stop's). The latest-action fields:
`new_position` the into stop's point, `new_stop_id` and `split_route_ids` `null`,
and the new `merge_into_stop_id` (`null` unless the latest action is a merge).

**The dry question.** `GET /position-reviews/{id}?stop_id=<candidate>` answers
`detour_m_after` as if the reviewed stop's calls were at that stop's position,
and `merge_problems: [{level, code, message}]` — what the `stop/merge` validation
would say, from the real validator, changing nothing. `merge_problems` is `null`
without `?stop_id=`.

Settled while implementing:

- **One more merge into the same stop is no conflict.** The brief's "any change
  to either stop" would have refused merging a second duplicate into the stop a
  first was just merged into, in one draft — the tool's main case (S and S2 both
  into C). So for the into stop only a change that moves, deletes or merges
  **it** away conflicts; its creation in the same draft, or another merge into
  it, does not. Two merges into one stop commit cleanly: conflicts are checked
  against the live rows before anything applies.
- Because it goes through the add path, the merge also writes the ordinary
  `change_added` (`/move` and `/split` insert their changes directly and do not).
  `before` is the merge's usual `{from, into, affected}`.
- Request refusals: 400 `invalid_merge` (blank `into_stop_id`, a `keep_name` that
  is not `into` / `from`), 400 `invalid_json` (an unknown field), 404
  `review_not_found` / `change_set_not_found`, 400 `feed_mismatch`, 409
  `change_set_not_draft`. Checks run in this order: the request; the draft and
  review as for move; 409 `draft_conflict`; 400 `review_has_problems` for the
  reviewed stop itself (missing, deleted, merged away, a station — the live data
  plus the draft); 400 `review_has_problems` from the merge's validation.
- `into_stop_id` is trimmed. The into stop's point is where the draft puts it
  (created in it), else where it is live. The note rule is move's.
- `?stop_id=` is sent without `lat`, `lon` or `route_ids` (400 `invalid_query`
  otherwise) and may carry `&change_set=<id>`: the question is then asked on top
  of that draft's changes (404 `change_set_not_found`, 400 `feed_mismatch`). A
  candidate that does not exist answers `merge_problems: [stop_not_found]` and
  `detour_m_after: null`, not a 404. The reviewed stop itself answers
  `merge_same_stop`.
- A committed merge's `draft_actions[].detour_m_after` is measured over the into
  stop's calls of the routes the merge moved (`before.affected`), since the
  merged stop has none left; its review's `problems` then say `stop_merged_away`
  and its `routes` are empty.
- `?auto_fix=` with any other value is 400 `invalid_auto_fix`; blank is no
  filter. A review without `evidence.auto_fix` matches no value, `none` included
  (`none` is the tool saying so), and is in no `auto_fix` count.
- Cost: the merge's validation applies the whole draft once per call (as `GET
  /change-sets/{id}` does), so a draft of N merges costs O(N) per added merge.
  The dry question on chennai_bus, without a draft: 6–8 ms.

## 9. Cleanup context — what is known about a stop or a route (2026-09-17)

Two reads for the dashboard's cleanup panels, viewer+, changing nothing.

| method | path | response |
|---|---|---|
| GET | `/feeds/{g}/stops/{stop_id}/context` | `{stop_id, detour_m, routes_measured, position_reviews: {pending, approved, committed, confirmed, items: [{review_id, status, reason}]}, same_name: [{stop_id, name, lat, lon, distance_m, route_count, parent_station, platform_code, description, similarity}], audit: [{audit_id, at, actor_email, action, change_set_id, detail}], open_drafts: [{change_set_id, title, status, change_id, entity, op}]}`. 404 `stop_not_found` |
| GET | `/feeds/{g}/routes/{route_id}/context` | `{route_id, stops_with_reviews: [{stop_id, sequence, review_id, status}], worst_detours: [{stop_id, name, sequence, detour_m}], audit: [...], open_drafts: [...]}` (same `audit` / `open_drafts` shapes). 404 `route_not_found` |

- `detour_m` is section 8's: the median over the stop's calls that have a served
  stop either side, by the same code (`position_reviews::calls`,
  `median_detour`); `null` when no call has. `routes_measured` is how many calls
  that median is over.
- `same_name`: other live stops (`location_type` 0, not deleted) within 5 km
  whose name has pg_trgm similarity ≥ 0.6 to this stop's, **or** is equal
  ignoring case and everything that is not a letter or digit; nearest first
  (`stop_id` breaks ties), at most 20. `similarity` is to two decimals — it can be
  under 0.6 for a name that matched by the second rule (`A N N A NAGAR`).
- `worst_detours`: the route's served calls whose own detour (between the served
  stops either side) is over 300 m, worst first, at most 5.

Settled while implementing:

- `position_reviews` counts and lists the stop's reviews in those four statuses,
  newest first; a `superseded` review was never looked at and is left out of
  both. `stops_with_reviews` has one entry per (row, review) in those statuses,
  in `sequence`, `review_id` order.
- `audit` is the latest 10 of, for a stop: every row naming it in
  `detail.stop_id` (all the `position_review_*` actions), `change_added` rows
  whose `detail.entity_key` is the stop (entity `stop` or `station`),
  `stop_merged` rows with it on either side, and the `change_set_committed` row
  of every change set holding a `stop` / `station` change keyed on it. For a
  route: `change_added` rows keyed on it (entity `route` or `route_stops`), and —
  of the change sets holding such a change — `change_set_committed` and the
  `position_review_split` rows whose `route_ids` name it. `change_updated` and
  `change_removed` rows carry only a change id and are not found.
- `open_drafts` is one entry per change, in the feed's `draft` / `submitted` /
  `approved` sets, latest-edited set first. For a stop: a `stop` or `station`
  change keyed on it, a `stop/merge` **into** it, or a `station` create / update
  listing it as a member. A `route_stops` change whose rows call at it is not
  listed (that would read every open stop list). For a route: a `route` or
  `route_stops` change keyed on it.
- A deleted stop still answers (its reviews and history are the point): no
  calls, so `detour_m: null` and `routes_measured: 0`.
- Indexes (`0011_context_indexes.sql`): the audit log by `detail->>'stop_id'`, by
  `detail->>'entity_key'`, and `stop_merged` by either side; the review queue by
  `(gtfs_id, stop_id)`. Same-named stops come from `gtfs_stop_latlon_idx` (a box a
  little over 5 km each way, then the true distance).
- Timing on the local chennai_bus data (debug build, warm): the stop context
  4–8 ms — 8 ms at the busiest stop (234 measured calls), 4–5 ms at the names the
  most stops share (10–12 same-named within 5 km); the route context 1.6–3 ms on
  the longest routes. `tests/editor_review_merge_flow.rs` prints them.

<!-- ===== section 10 begins: dashboard round 4 (UX). Self-contained; sections 2, 3 and 8 are edited elsewhere. ===== -->
## 10. Dashboard requirements added 2026-09-17, round 4 (reviewer UX)

Dashboard only (`editor-ui/`), apart from the read endpoints and the merge action
in 10.5, which the API provides. Every item has checks in `dev/ui_smoke.mjs`
(`round4Flows`; `node dev/ui_smoke.mjs --round4` runs only these) against
`dev/mock_server.py` (its block "round 4 (UX)").

1. **What the map shows.** A "Show" control under the zoom buttons ticks
   Stations, Routes and Stops on or off, each on its own. The choice is a
   per-browser preference (`localStorage`, key `mapLayers` of the editor's
   preferences; the page works when storage throws, it then simply starts with
   everything shown). A hidden kind stays hidden as the map moves, zooms and loads
   new stops, its name labels and count badges included. What the panel is about
   is never lost: the open stop (or station) is still drawn when its kind is off,
   every stop is drawn while one has to be clicked (picking a stop, choosing a
   station's stops), and an open route that is hidden is named as hidden; the
   control says which of these applies.
2. **A station once, not each of its platforms.** Where a panel lists stops near
   or on a stop's point — "Other stops within 60 m" on the stop page, "Stops that
   shared its point" on a coordinate review, the same-named stops of 10.5, the
   suggestions in the station editor — a stop that is already a platform of a
   station is not also a bare entry: the station is listed once, linked, with "N of
   these are already platforms of <station>", and its platforms fold under it (one
   click away, with their own Merge… buttons). Station names come from the rows'
   `parent_station` (`parent` on a stop detail); a name not at hand is read from
   the stop endpoint once per panel. In the station editor such stops are not
   offered at all, since a stop has one parent.
3. **Undo and redo of what is not in a draft.** Ctrl/Cmd+Z undoes, Ctrl/Cmd+
   Shift+Z and Ctrl/Cmd+Y redo: a pin placed or dragged (stop editor, New stop,
   coordinate review, station point), ticked routes on a review, a select, every
   row operation of the stop list editor (add, change stop, move, remove, type,
   stage, renumber, fix), a station's stops and labels, the choices of a merge,
   a suggested or discarded map line, and a text field as a whole once it is left.
   It never touches data: a change already in a draft is removed on the draft
   page. While the caret is in a text field the shortcut is left to the browser,
   so typing undoes natively. One in-memory stack per panel (`js/undo.js`;
   `state.js` had nothing to reuse): the router drops it on every navigation, and
   a panel clears it when its state is saved into a draft. A short hint says what
   happened ("Undid: moved the pin"), and Undo / Redo buttons sit wherever a pin
   or a stop list is edited.
4. **The trail.** A drill-down (route → stop → another route → …) leaves a trail
   under the top bar, "Map › 45B › Luz › 12C", with one "‹ Back" control
   (`js/trail.js`, beside the hash router). It is not the browser history: each
   place is in it once; opening a place already in it pops back to it; clicking a
   crumb pops to it; a link in the top bar, the brand or a search result starts a
   new trail; a top-level page (Map, Coordinates, Stations, Drafts, …) is the root
   of one. At most 8 places (the oldest after the root go). `sessionStorage` only,
   so it survives a reload and ends with the tab. A panel keeps its own "Back to
   search" only when the trail has nowhere to go back to.
5. **Cleanup context, the tool's verdict, candidates and merge.**
   - `GET /feeds/{g}/stops/{stop_id}/context` → `{stop_id, detour_m,
     routes_measured, position_reviews: {pending, approved, committed, confirmed,
     items: [{review_id, status, reason}]}, same_name: [{stop_id, name, lat, lon,
     distance_m, route_count, parent_station, similarity}], audit: [{audit_id, at,
     actor_email, action, change_set_id, detail}], open_drafts: [{change_set_id,
     title, status, change_id, entity, op}]}`; `GET /feeds/{g}/routes/{route_id}/
     context` → `{route_id, stops_with_reviews: [{stop_id, sequence, review_id,
     status}], worst_detours: [{stop_id, name, sequence, detour_m}], audit,
     open_drafts}`. The stop and route panels show them as "Cleanup context": the
     detour with what it means in plain words, the coordinate reviews naming the
     stop (linked into the Coordinates page), same-named stops nearby (listed with
     distance and route count, drawn faintly on the map, stations once as in 2),
     open drafts touching it, and recent history in the History page's words
     (`ACTION_LABEL`). The route's stop list marks rows whose stop has a pending
     review, and its context lists the longest detours. **A server without these
     endpoints answers 404: the section is then not shown, with no error.**
   - A review's `evidence` may carry `same_name_candidates: [{stop_id, name, lat,
     lon, distance_m, name_similarity, route_count, detour_m_after, shares_route,
     verdict: "fits" | "no_fit"}]` and `auto_fix: {action: "merge" | "move" |
     "choose" | "none", into_stop_id?, lat?, lon?, detour_m, detour_m_after?, reason,
     tool, threshold_m}`. The review panel shows the verdict as a banner ("The tool
     suggests merging into X — detour 2.4 km → 40 m", with the reason), and the
     candidates as a numbered list and numbered map markers (teal where the routes
     fit), each with "Use this position" (sets the pin: a move) and "Merge into
     this stop…".
   - `POST /position-reviews/{id}/merge` `{change_set_id, into_stop_id, keep_name?,
     note?}` → the review detail plus `warnings`; 400 `review_has_problems`
     (`details.problems`), 409 `draft_conflict`. `draft_actions` may then hold
     `{kind: "merge", change_id, into_stop_id, lat, lon, detour_m_after}`, drawn
     and listed like a move or a split; a merged review offers no move or split.
   - The list filters by `?auto_fix=merge|move|choose|none` with chips that carry
     the summary's `auto_fix: {merge, move, choose, none}` counts; without those
     counts (an older server) there are no chips.
6. **Stops sharing a point.** Stops with exactly the same coordinate (about a
   thousand points in Chennai, up to 61 stops on one) are one marker with a count
   badge; clicking it lists them by name, id and route count so any can be chosen,
   in every mode that clicks a stop (opening, picking, a station's stops). A stop
   alone opens directly, as does a click that only wants the place (a review's
   pin). The chooser counts what is shown (10.1), and differently named stops on
   one point share a label at zoom ≥ 17 ("NAME +2").
7. **Pending in the draft, on the page it changes.** A change added to the active
   draft is not live, but its pages show the entity as the draft leaves it,
   labelled "Pending in draft “<title>”, not live", with the live value beside it
   ("live: …", struck through): a stop's drafted name, position and labels (the
   map marks the drafted place and ghosts the live one, and the area stops are
   drawn where the draft puts them); a stop that will be merged into X, and X
   saying which stop is merged into it; a stop to be deleted; a stop or a station
   that exists only in the draft (both can be opened); a station's drafted name,
   point and members, and a member that joins or leaves; a route's drafted number,
   name, colour and map line, and its drafted stop list (from `GET
   /change-sets/{id}/preview/routes/{route_id}`, rows marked added / moved /
   changed, removed stops named, the live line dashed underneath) with "Show what
   is live now". It is a read-side overlay in one module, `js/overlay.js`, which
   the coordinate review panel uses too (its `draft_actions` have the same shape
   as the overlay's actions). The cache is the active draft the dashboard already
   holds: every mutation it makes replaces that object, as does choosing another
   draft, and the overlay rebuilds on that.
<!-- ===== section 10 ends ===== -->

## 11. Stop details, bulk updates and station links (2026-09-17)

Asked for on master: "Add default platform name for solo stops also - the next
stop, like 'Towards <this>' - and a stop description for stops and stations too.
Add a polyline to represent stations and corresponding stops." The defaults
(about 10,000 stop updates) are computed by nandi and arrive **as a draft**, like
every other write: through the bulk kind below, then submit, approve (someone
else) and commit.

### 11.1 `description` on stops and stations

`gtfs_stop.description` (`0012_stop_description.sql`, nullable text): where the
stop is, in words — GTFS `stop_desc`.

- **Read shape**: `description` is part of the stop row, so it is in every place
  the row is: `/stops`, `/stops/{id}` and its `children`, `nearby` and `parent`,
  a change's `before`, a coordinate review's `stop`, a merge's `from` / `into`.
  The cleanup context's `same_name` items carry `platform_code` and
  `description` too.
- **Writes**: `stop/update`, `stop/create`, `station/create` and
  `station/update` take `description`. Left out = as it is; `null` or blank =
  cleared; at most 500 characters once trimmed, else the validation error
  `description_too_long` (400 `invalid_change`, `details.code`, when the change
  is added; the same finding in a bulk row). It applies and conflicts exactly as
  `platform_code` does: the change is based on the stop's `row_version`.
  `stop/merge` leaves the kept stop's description as it is.
- **Public API**: a DB-backed feed's stop JSON gains **`description`** (no
  preprocessed field existed for `stop_desc`, so this is the name), emitted only
  when the stop has one. Parity with the preprocessed load is therefore unchanged
  for every stop without a description; a feed served from preprocessed data
  never has the field.
- **A platform label needs no station.** `platform_code` on a stop with no
  `parent_station` applies, is in the read shape, and is served as the public
  `platformCode` (`stationId` stays `null`). Nothing in the editor assumed
  otherwise; `tests/editor_stop_details_flow.rs` proves it end to end.

Settled while implementing:

- `platform_code` and `description` are stored **trimmed**, and a blank one is
  stored as `NULL` — on `stop/update` and `stop/create` too, where a blank
  `platform_code` used to be stored as `""` (a station's `members[]` already
  cleared on blank).
- `stop/update` and `stop/create` now refuse a `platform_code` longer than 120
  characters with `invalid_platform_code`, the rule a station's
  `members[].platform_code` and the proposals always had; before, only those two
  paths checked it.
- A station never has a `platform_code` of its own: `station/*` with one is 400
  `invalid_change`, `details.code = "invalid_payload"`, as any unknown field is.
- The loader also treats a blank description in the table as none (a row written
  around the editor), so the public field is never `""`.
- Lengths count characters (not bytes) after trimming.

### 11.2 Bulk kind `stop_updates`

`POST /change-sets/{id}/bulk` with `kind: "stop_updates"` (section 5: same cap of
5,000 rows, same response, same `dry_run` semantics, one transaction, one
`bulk_imported` audit row). A row is

```json
{"stop_id": "29db2391b0", "platform_code": "Towards THIRUPORUR", "description": "…", "name": "…"}
```

`stop_id` is required; `platform_code`, `description` and `name` are each
optional, and at least one must be given. Each row becomes one `stop/update`
whose `after` holds **exactly the cells given**, `base_row_version` the stop's
current `row_version`, and `before` the stop row in read shape — the change
`POST /change-sets/{id}/changes` would have stored. A blank or `null` cell is
"not given": **clearing a field through an upload is out of scope** (clear it
with a single `stop/update` carrying `null`).

Row findings (`messages[].code`), errors unless said:

| code | when |
|---|---|
| `invalid_row` | not an object; a column that is not one of the four; a cell that is not text (a number is taken as its text); `stop_id` missing or blank |
| `nothing_to_update` | none of `platform_code`, `description`, `name` is given |
| `duplicate_in_upload` | the same `stop_id` on more than one row (on every row involved) |
| `stop_not_found` | no such stop, live or created earlier in the draft |
| `stop_deleted` | the stop is deleted (a stop merged away by a commit is deleted), or deleted earlier in the draft |
| `stop_merged_away` | an earlier change in the draft merges the stop into another |
| `platform_code_on_station` | `stop_id` is a station and the row gives `platform_code` |
| `invalid_platform_code`, `description_too_long`, `invalid_payload` | the single change's shape check: 120 / 500 characters |
| `unchanged` (**warning**) | every given value already equals the stop's, **taking the draft as applying**. The row becomes **no change**: `change` is `null`, and `summary.unchanged` counts it |
| `stop_already_in_draft` (**warning**) | the draft already has an update of that stop (and the row differs from what it leaves): the row is still a change, applied after it |

A station id is accepted for `description` and `name`; the row becomes a
`station/update` (its `before` lists `member_stop_ids` / `members` as a single
station change's does).

Settled while implementing:

- **Re-running an upload is idempotent.** Once its changes are in the draft,
  the same rows are all `unchanged`; a real run with no change to add writes
  nothing — no change, no `updated_at`, no audit row — and still answers 200 with
  `summary.changes: 0` and the set as `change_set`.
- `summary.unchanged` is in a `stop_updates` response only; the other kinds'
  summaries are as they were. (Every kind's response also names its `kind`, as
  it always has.)
- Every row that names a known stop also carries `stop: {name, lat, lon,
  platform_code, description, parent_station}` — the stop as it is now, the
  draft applied — so the preview can show what changes, and where, without a
  read per row. Rows of the other kinds have no `stop`.
- Values are compared trimmed; a name is compared as it is spelt (case matters).
- What "the draft as applying" covers (`DraftView::texts_after`): earlier
  `stop/update`, `station/update`, `stop/create` and `station/create` of the
  stop; a station change's `members[].platform_code`; a merge into the stop that
  hands over its name (`keep_name: "from"`) or, with its station, its label.
- A stop created earlier in the draft can be updated: no `base_row_version`, and
  `before` is the create's `after` (section 5's rule).
- The editor API takes a JSON body of at most 8 MiB (as for every kind): 5,000
  rows fit unless their descriptions average well over a kilobyte; split such a
  file.
- Two updates of one stop in one draft do not conflict with each other: commit
  checks every change's base against the live rows before anything applies.
- **Cost.** The upload is validated once, not per row against the draft: one
  query for the stops it names, one read of the draft's changes, one in-memory
  replay of them (O(draft + upload)), and one `INSERT … SELECT FROM UNNEST`.
  On the local chennai_bus copy (debug build, `tests/editor_stop_details_flow.rs`
  prints them): dry run of 5,000 rows 0.28–0.30 s; the same 5,000 onto a draft
  already holding 5,000 changes 0.27 s (2,000 new rows: 0.14 s) — no dearer than
  onto an empty draft; the real run of 5,000 rows 2.0 s, nearly all of it the set
  detail it returns (which applies the whole draft once, as `GET
  /change-sets/{id}` does), and 2,000 more onto those 5,000 2.3 s; submit of the
  7,000-change draft 3.7 s, its commit 1.6 s. nandi's 10,000 defaults are two
  uploads into one draft.

### 11.3 Station links on the map, and `platform_count`

The map ties every station in view to each of its platforms with a thin line,
so it is plain which stops belong to which station.

- Drawn from zoom 15 when **Stations** and the new **Station links** tick of the
  Show control are on (`mapLayers.links` of the editor's preferences, remembered
  like the others), to the platforms that are drawn — so not while Stops is off,
  and the control says so. The lines are in the pane under the markers and take
  no clicks.
- The open station's lines — or the open platform's station's — stand out; the
  rest are faint. On **Coordinates to review** the reviewed stop's station stands
  out, on **Stations to review** the suggestion's own station once it exists;
  both pages draw the lines for every station in view, since they are part of
  the map's stop layer.
- They follow the active draft through `js/overlay.js`: a stop or station the
  draft moves is tied where the draft puts it; a stop a drafted station change
  takes in is tied to that station (also one the draft creates), one it takes out
  is not. Each of those is drawn in the draft's amber, dashed, and the stop's
  tooltip says it joins or leaves the station "in draft …, not live".
- Data: the stops the map already loads for its area (`parent_station`). The stop
  rows of a list and of a detail carry **`platform_count`**, so the map knows
  whether it has all of a station's platforms; only when it has not does it read
  `GET /feeds/{g}/stops?station=<id>` — or `GET /feeds/{g}/stops/{id}` when the
  station itself is outside the area — once per station, feed and page session,
  at most four at a time, never again on a pan. A stop or station page the person
  opens fills the same cache from what it read. A server without
  `platform_count` is asked once per station in view.

Settled while implementing:

- `platform_count` counts live (not deleted) platforms; it is on every row of
  `/stops` and on `/stops/{id}` (where it equals `children.length`), and is not
  part of a change's `before`.
- A failed read is remembered for 30 s and then tried again; nothing is cached
  for it.

### 11.4 Dashboard

1. **Stop page, stop editor, New stop; station page, station editor, New
   station**: a "Description" textarea (500 at most, with a counter). The
   platform label is offered to every stop, in a station or not, with the
   placeholder "Towards <next stop>" and the help "What passengers see as the
   platform or direction at this stop…". Both go through the ordinary draft
   change and show as pending in the draft (`js/overlay.js`, "Changes the platform
   label, description"), the live value beside them. Putting a drafted value back
   to what is live is sent too, so the draft's change follows the form.
2. The platform label and the description are in the stop panel's header, in the
   hover titles of the lists of stops (a station's stops, "Other stops within 60
   m", same-named stops nearby), in the marker's tooltip on the map, and in the
   draft's diff.
3. **Import**: a fourth kind, "Stop details (platform label, description)" —
   template `stop_updates-template.csv` (`stop_id, platform_code, description,
   name`), parsed in the browser, previewed with `dry_run: true` in the same
   table (plus a "Stop now" column from each row's `stop`) and on a map of the
   stops the file names, "N unchanged, not added" among the counts, then "Add N
   changes to draft". A file whose every row is unchanged says "Nothing to add".

Tests: `tests/editor_stop_details_flow.rs` (registered in
`scripts/editor_flow_test.sh`); `dev/ui_smoke.mjs` `round5Flows` (`node
dev/ui_smoke.mjs --round5` runs only these) against `dev/mock_server.py`
(`seed_round5`, `bulk_stop_updates`).

## 12. Webhooks, and knowing when an edit is live everywhere (2026-09-18)

GIMS calls a URL when something happens to a feed. It is general plumbing — a
webhook row picks one `event`, and the dispatcher delivers it exactly once with
retries — but it exists for one problem in particular.

**The problem.** The frontline layer is static files on S3 behind CloudFront,
rebuilt and invalidated by a Jenkins job. That job has to run *after* an edit is
live on every pod, not when it was committed. A commit bumps `gtfs_feed.version`
and each pod notices within its poll interval, so for a few seconds the fleet is
mixed. Fire on commit and a pod still holding the previous version can answer the
request that the freshly invalidated CloudFront passes through — and CloudFront
caches that stale answer again, for as long as its TTL says.

**The answer.** Each pod reports the version it has loaded, and the webhook fires
once every live pod is at the committed version. No sidecar: a sidecar would have
to infer a pod's cache state from outside, and the process that owns the cache
can simply say.

```
commit ──▶ gtfs_feed.version = N
             │
   each pod polls (5 s), rebuilds the feed, then writes its own row:
             ▼
   gtfs_pod_feed_state   pod-a → N     pod-b → N-1     pod-c → N
             │                            ▲ not there yet: nothing fires
             │  …one poll later, pod-b reaches N and the fleet settles
             ▼
   every pod tries to INSERT the delivery; the unique index lets one through
             ▼
   that pod POSTs the webhook ──▶ Jenkins ──▶ rebuild S3 + invalidate CloudFront
```

### 12.1 Events

| `event` | fires when | typical use |
| --- | --- | --- |
| `feed_in_sync` | every live pod is serving the committed version, and has been for `settle_seconds` | rebuild a downstream cache |
| `feed_committed` | a draft was committed, at once, without waiting for the pods | notify a chat channel |
| `feed_reload_failed` | a pod could not load a version and is serving older data | alert |

A webhook only ever fires for a version that appeared **after** it was
configured, so adding one to a quiet feed does not immediately trigger a
rebuild.

### 12.2 Exactly once, without a leader

Every pod runs the same dispatcher and they all notice the same moment. They all
try to insert the delivery row; the partial unique index on `(webhook_id,
feed_version) WHERE kind = 'event'` lets exactly one through, and only that pod
sends the request. There is no leader to elect and nothing to fail over. A
delivery claimed by a pod that dies mid-request is reclaimed by another pod after
`CLAIM_STALE_SECONDS`, so a kill costs a retry, not a delivery.

Skipped versions are normal. If two commits land faster than the pods reload,
the fleet settles on the newer one and only that version gets a delivery — one
invalidation per settled state, not one per commit.

### 12.3 What holds a delivery back, and what gives up

- **A laggard pod** — any live pod below the committed version. The delivery
  waits. `GET /feeds/{g}/cache-state` names it.
- **A silent pod** — no heartbeat for `stale_after_seconds` (default 60). It is
  treated as gone, not as a laggard: otherwise one dead pod would suppress every
  future delivery in silence.
- **A pod on preprocessed data** — counted as neither. It is healthy, but this
  feed's version means nothing to it.
- **No live pod at all** — never fires. Firing would tell the frontline to
  rebuild from data nothing is actually serving.

After `give_up_after_seconds` (default 1800) an undelivered version is recorded
as an **abandoned** delivery, with the laggards named. It is deliberately not
fired anyway: rebuilding a downstream cache from a fleet we know is inconsistent
is the failure this whole mechanism exists to prevent. The abandonment is
visible in the dashboard and the log; press **Test** once the fleet is healthy.

### 12.4 Credentials

`url` and every value in `headers` may contain placeholders:

- `${NAME}` / `${env:NAME}` — an environment variable **of the pod**, which is
  where the credential lives: in the Kubernetes secret the pod already mounts.
- `${event:field}` — `gtfs_id`, `feed_version`, `event`, `delivery_id`,
  `webhook`, `pod_count`, `fired_at`.

So a Jenkins hook is stored as

```
https://<jenkins host>/job/frontline-rebuild/buildWithParameters?token=${JENKINS_TOKEN}&FEED=${event:gtfs_id}
```

The token is never written to the database, never returned by the API, and never
reaches the audit log. An **unresolved** placeholder fails the delivery rather
than sending the literal text, and is caught when the URL is saved, not at the
first delivery. An error message has the URL and its query string removed before
it is stored.

The request body is the built-in JSON payload (`event`, `gtfs_id`,
`feed_version`, `delivery_id`, `webhook`, `pod_count`, `fired_at`) unless `body`
is set, in which case that object is sent with `${...}` resolved in every string
leaf. `GET` sends no body.

### 12.5 Where a webhook may point

A URL's host must match the **allow-list** — checked when it is saved and again
when the request is about to go out, because the list can be tightened after a
webhook was configured. An entry written `.example.com` matches that domain and
its subdomains.

The allow-list, and the on/off switch beside it, are the *webhook policy*, and
they live in **two places with one rule between them**: the
`gtfs_webhook_settings` row wins, and the dhall config is only the seed used
while no row has been saved. That is exactly how `gtfs_feed.data_source`
supersedes the static `gtfs_db_feeds` list (section 1), deliberately — this
system answers "which of these two wins" once, not twice.

```dhall
gtfs_webhooks_enabled = True,                                -- the seed
gtfs_webhook_allowed_hosts = [".internal.svc.movingtech.net"],
gtfs_pod_id = None Text,   -- defaults to $POD_NAME, then the hostname
```

An admin edits the policy on the Delivery page (`#/delivery`), which writes the
row; it is read by every pod **on each version poll**, so turning webhooks on, or
adding the host that a delivery has been failing on, takes effect within a poll
interval with no restart and no configmap edit. `check_host` reads the same live
list at the moment of the request, so a host removed from it stops being called
by webhooks that were configured while it was there.

Empty allow-list means no webhook can fire, whether the switch is on or off: the
feature fails closed, and turning it on before deciding where it may point is a
legal half-step that sends nothing. With the switch off pods do not even report
their cache state, so a deployment that has neither the row nor
`gtfs_webhooks_enabled = True` is completely unaffected by all of this.

A host is a plain host name (`jenkins.example.com`), or one with a leading dot
for a domain and its subdomains (`.example.com`). No scheme, port, path, space or
wildcard — none of those could ever match, so they are refused as the typos they
are (400 `invalid_host`) rather than quietly stripped. Entries are lowercased,
trimmed and de-duplicated, and the list holds at most 64.

**Why this is not a draft change.** Everything about a *feed* goes through a
draft a second person approves. A webhook is not feed data, and a draft would not
address the actual risk, which is GIMS being pointed at a host it should not
call. That is answered by the allow-list, by the policy being an **admin's** to
change, and by every change being audited (`webhook_created`, `webhook_updated`,
`webhook_deleted`, `webhook_tested`, `webhook_settings_updated`, the last
carrying the whole policy before and after).

**What this gives up.** Before the row existed, the deployment was a hard ceiling:
an admin chose the URL, and could not widen the list it had to sit inside. That
ceiling is gone. A compromised or careless admin account can now add a host and
point GIMS at it without anyone editing the configmap, and the audit row is what
you have afterwards rather than a second pair of eyes beforehand. It was traded
knowingly: the list is one ops discover they need an entry in *while a delivery is
failing*, and an engineer editing a shared configmap and restarting every pod was
both slower and, in practice, less reviewed than it looked. The credential is not
part of the trade — a URL's secret stays a `${PLACEHOLDER}` resolved from the
pod's environment (12.4), so a host added here cannot be handed a token the pods
do not already hold for it.

`gtfs_pod_id` must differ per pod. Two pods sharing one id overwrite each other's
heartbeat, the fleet looks smaller than it is, and a webhook fires early.

### 12.6 API

| | |
| --- | --- |
| `GET /feeds/{g}/cache-state` | viewer+. The feed's version, each pod's loaded version, `in_sync`, and `waiting_for` |
| `GET /webhook-settings` | viewer+ → `{policy, config: {enabled, allowed_hosts}, max_allowed_hosts}`. `config` is the deployment's seed, shown so the page can say what saving takes over from |
| `PUT /webhook-settings` | **admin**. `{enabled?, allowed_hosts?}` → the same shape. Only the fields sent are changed; the rest keep what is in force. Audited `webhook_settings_updated` |
| `GET /feeds/{g}/webhooks` | viewer+. The rows, plus `policy` and `events` |
| `POST /feeds/{g}/webhooks` | **admin**. `{name, event?, url, method?, headers?, body?, enabled?, …}` → 201 |
| `PATCH /webhooks/{id}` | **admin**. Only the fields sent are changed |
| `DELETE /webhooks/{id}` | **admin** |
| `POST /webhooks/{id}/test` | **admin**. Queues a `kind = 'test'` delivery, sent like a real one, which does not consume the version's delivery |
| `GET /feeds/{g}/webhook-deliveries?limit=` | viewer+. History with status, attempts, response code and error |

`policy` everywhere is the one in force: `{enabled, allowed_hosts, active,
source: "database"|"config", updated_at, updated_by}`. `source` says which of
the two it came from — the row, or the deployment seed that is still standing in
for one — and `active` is `enabled && allowed_hosts` non-empty, which is the
condition anything fires under.

Errors: `host_not_allowed`, `invalid_url` (a placeholder that cannot be
resolved), `invalid_host` (an allow-list entry that is not a host name),
`invalid_event`, `invalid_method`, `invalid_headers`, `out_of_range`,
`duplicate_name`, `webhooks_inactive`, `webhook_not_found`.

Retries: 30 s, 1 m, 2 m, 4 m, 8 m, then every 15 m, up to `max_attempts`
(default 5). The receiver here is a build system, so retrying for a long while
beats dropping the request: a missed delivery means CloudFront serves yesterday's
data until someone notices.

### 12.7 Schema and tests

`db/gtfs_editor/0013_webhooks.sql` — `gtfs_pod_feed_state`, `gtfs_webhook`,
`gtfs_webhook_delivery`. Safe to run twice. **Apply it before rolling out an
image with `gtfs_webhooks_enabled = True`**; without it the pods log one line and
carry on serving, and no webhook can fire.

`db/gtfs_editor/0016_webhook_settings.sql` — `gtfs_webhook_settings`, the policy
row of 12.5. One row, ever: `singleton boolean PRIMARY KEY CHECK (singleton)`,
because the policy is the deployment's and a second row would raise the question
of which one is in force. Safe to run twice. Without it every pod falls back to
the dhall values — the state before this existed — and logs that once.

Tests: `tests/editor_webhook_flow.rs` and
`tests/editor_webhook_settings_flow.rs` (both registered in
`scripts/editor_flow_test.sh`) run the whole path against a real Postgres and a
real HTTP receiver — two pods dispatching at the same instant to prove the
delivery goes out exactly once, and a saved policy turning the feature on, taking
the deployment's own host away and stopping a call at send time. The fleet
arithmetic, the precedence rule, the host validation, the placeholders and the
backoff are unit tested in `src/services/webhook.rs`.


## 17. A map line from GPS (2026-09-21)

A route's map line is `gtfs_route.encoded_polyline` (Google polyline, precision
5) and `polyline_source`. Only 10 of chennai_bus's 5,567 routes have one, and the
one way to get one - a road route through the stops (`polyline:osrm`) - failed
on master with "OSRM could not route through these stops" and no reason. The
buses know the way even where the stops or the router's map do not, so the
editor can now also propose the path this route's buses actually drove.

```
POST /feeds/{g}/routes/{route_id}/polyline:gps?change_set=
```

Editor role, like `polyline:osrm`. With `change_set` it asks about the route as
that draft has it (its route number and its stops); without, the live route.
Nothing is written: the dashboard puts the line into the draft as a `route`
change with `polyline_source: "gps"`, exactly as it does the OSRM suggestion,
and it goes live when someone else approves and commits the draft.

```json
{
  "route_id": "115",
  "encoded_polyline": "…",
  "polyline_source": "gps",
  "saved": false,
  "evidence": {
    "from": "2026-09-08", "to": "2026-09-21", "days": 14,
    "route_number": "21G", "stops": 50,
    "bus_days": 30, "pings": 221034, "points_read": 81002, "truncated": false, "queries": 15,
    "buses": 12, "runs_seen": 190, "runs_used": 31,
    "stop_coverage": 0.94,
    "matched": "osrm", "matched_share": 0.992, "osrm_detours_skipped": 3,
    "points": 1747, "length_m": 38050, "cached": false,
    "consolidation": {"cells": 3120, "cells_kept": 2410, "samples_off_corridor": 212,
                      "degraded": false, "runs_same_direction": 31, "runs_opposite_direction": 0}
  }
}
```

| field | meaning |
| --- | --- |
| `from`, `to` | the service days (Indian time) read: the last `days`, today included |
| `route_number` | the route's `short_name`, which is how the pings name the route |
| `bus_days`, `pings`, `points_read` | bus-days read, the raw pings behind them, the averaged points they became |
| `buses` | distinct vehicles among the runs used |
| `runs_seen`, `runs_used` | bus runs found (split at feed gaps and terminal dwells), and those that passed this route's stops in order |
| `stop_coverage` | share of the route's served stops within 30 m of the line |
| `matched` | `osrm` - snapped to roads all through; `partial` - some stretches kept the GPS geometry; `none` - OSRM matched nothing or is not configured: the GPS path as recorded |
| `matched_share` | share of the line's length OSRM matched |
| `osrm_detours_skipped` | stretches OSRM matched with a loop the buses did not drive, where the GPS path was kept (17.3) |
| `osrm_error` | the first thing OSRM said, when something went wrong |
| `cached` | this answer came from the cache (17.4) |

Errors: `503 gps_unavailable` (GPS not configured, or not for this feed), `422
gps_no_route_number` (the route has no `short_name` to find its buses by), `422
gps_not_enough_stops` (fewer than two served stops with a position), `422
gps_not_enough_runs` (fewer than 3 runs passed the stops in order; `details` holds
the evidence counts above plus `min_runs`), `504 gps_timeout` (the whole
suggestion took longer than its limit, 55 s by default), `502 gps_query_failed`
(ClickHouse answered with an error, or could not be reached).

### 17.1 Which runs count

A route NUMBER covers both directions and several variants, each its own
route_id, and the operator-entered label is sometimes wrong. An earlier attempt
that filtered by the number and averaged everything drew paths missing nearly all
their stops. So a run counts only if it passes **at least 70% of this route's
served stops, each within 60 m, in increasing sequence order**, with no more than
30 minutes between two stops it matched. That one rule rejects the other
direction (it meets the stops backwards), the other variants (they miss the stops
of the stretch they do not share) and a mislabelled bus (it misses most of them).
Close stops may be met up to 30 m out of order. A kept run is cut from its first
matched stop to its last.

Before that, a bus-day's points are cleaned: impossible jumps (over 110 km/h from
the last kept point) are dropped, and so are **spikes** - one or two points that
go more than 80 m off the line between their neighbours and come straight back.
An averaged 20-second point carrying one wild fix is exactly that, and no speed
limit catches it. The day is then split into runs at gaps in the feed longer than
10 minutes, and where the bus stood within 100 m for 5 minutes - a terminus -
cutting at the spot it stood, not at the edge of the radius. Runs under 10 points
or 1 km of extent are dropped.

### 17.2 Consolidation

The ideas are nandi's `scripts/chennai-bus/src/cleanup/s0_gps_ingest.py`, ported:

1. Every kept run is filled in every 15 m between its points (not across a gap
   of more than 600 m, which would be a guess). A point the bus reported weighs
   five times one filled in: between two pings 100 m apart a chord cuts every
   corner, and the ping is where the bus was.
2. A reference run is elected: the one through the most-travelled 30 m grid
   cells (log-weighted, distinct cells, so a bus stuck in traffic does not win).
3. Every sample of every run is placed along the reference **monotonically** -
   near where its previous sample was, or a little further on past a detour the
   reference took - starting near the run's first matched stop. That is what
   keeps a route that ends where it began from folding onto itself.
4. Within each grid cell, each separate pass (the same road driven out and back
   is two passes) becomes one weighted mean point. Passes that fewer than a
   quarter of the median number of runs went through (at least two, with four
   runs or more) are dropped: a detour one bus took is an order of magnitude
   thinner than the road they all drove. Fewer than 10 left and every pass is
   kept (`degraded: true`).
5. Passes at the same arc length (the two carriageways of one road) are merged,
   the line is ordered along the reference, oriented first stop to last, and
   simplified (Douglas-Peucker, 5 m).

On a synthetic fleet - ten runs of a 7 km corridor with right angles, a long
curve and a dog-leg, 8 m of noise and 2% wild fixes, plus runs the other way, a
variant that leaves half way and buses of another route under this label - the
line lands a median 2.0 m and a 90th percentile 5.2 m from the true road, covers
99% of it within 15 m and is within 0.5% of its length
(`editor::gps_line::tests::consolidated_line_lands_on_the_true_corridor`).

### 17.3 Snapping with OSRM

The consolidated path is resampled every 50 m and sent to OSRM
`/match/v1/driving` in chunks of at most 100 points overlapping by 8, with
`radiuses` 25 m, `overview=full&geometries=polyline&tidy=true&gaps=ignore`.
Neighbouring chunks meet in the middle of their overlap, and the chunks are
stitched point by point: between two points OSRM put in the same matching, the
matched road; a point it tidied away is bridged along the matching.

Where OSRM cannot match - a chunk that fails, or two points in different
matchings - that stretch keeps the GPS geometry. OSRM down or not configured, the
line is the GPS path as it is (`matched: none`).

OSRM also **invents detours**: a point put on the other carriageway, then a loop
through the next U-turn to reach it. On real 21G data a 1.9 km stretch came back
as an 8 km matching with confidence 0. A matched stretch longer than 1.5 times
the GPS stretch it stands for plus 60 m is therefore refused and the GPS
geometry kept (`osrm_detours_skipped`). `matched` is `osrm` when every chunk
answered and at least 99% of the length was matched.

### 17.4 Cost, safety and the cache

The pings are in ClickHouse, `atlas_kafka.amnex_direct_data`: a production
cluster that other teams share, read with a user that can write. GIMS reads it
only through `services::clickhouse_reader`, which enforces read-only three times,
as nandi's `ch_client.py` does:

- `readonly=2` is sent as a setting on every request, so the server refuses any
  write whatever the statement says (2, not 1, so that `max_execution_time` can
  still be set);
- a statement must open with `SELECT` or `WITH` and carry a `LIMIT`; a write
  keyword, `SETTINGS`, `FORMAT`, `INTO OUTFILE`, a `SYSTEM` command and the table
  functions that reach outside the cluster (`url()`, `s3()`, `remote()`, …, which
  readonly=2 still allows) are refused before anything is sent;
- a statement holding a `;` anywhere is refused.

It is unhurried: one query at a time per pod, 350 ms between queries,
`max_execution_time` 30 s, `max_threads=2`, low `priority`; one suggestion at a
time per pod (a second waits for the first, and then finds its answer in the
cache when it was for the same route). Answers come back as `TabSeparated` (JSON
formats stall on this cluster), and the password goes as HTTP basic auth: it is
in no URL, log line, error message or `Debug` output, and a ClickHouse error
naming the user is scrubbed.

The table's sort key is the timestamp alone, so a route filter prunes nothing:
**every query is bounded by time first**, and by `timestamp <= now()` (device
clocks emit far-future timestamps), then by the route label, a box around the
route's stops (+1 km), and a `LIMIT`. Raw pings are never pulled:

1. **Bus-days** - one query over the window: which `deviceId`s carried the route
   number (`routeNumber`, trimmed, case-insensitive) with at least 120 pings in
   the box, per Indian day, the busiest few per day (`LIMIT n BY day`). At most
   `max_bus_days` (30) are read, spread over the days: the busiest of each day in
   turn, newest first.
2. **Tracks** - one query per day for that day's buses: their pings in the box,
   labelled with this route number or with none (a quarter carry no label),
   averaged in ClickHouse to one point per 20 s, and packed one device-hour per
   row as `t,lat,lon|…` - so a day is a few dozen rows (some network paths to the
   cluster stall above ~300 rows; pages are 100 rows). Reading stops at 150,000
   points (`truncated`).

So a suggestion is about 15 small queries - measured on real data from a laptop,
4 days and 6 bus-days of 21G: 5 queries, 44,387 pings, 7-11 s.

The cache is in memory, per pod, keyed by (feed, route, a hash of the route
number and its stops' ids and positions, the day). It keeps the GPS half - the
consolidated path and its counts, or "not enough runs" - and, once OSRM has
snapped it all through, the whole answer. A line OSRM failed to snap is not kept
as the answer: the next click tries OSRM again, from the cached GPS half, without
asking ClickHouse. Editing the route's stops or number changes the key. 256
entries, the oldest dropped first.

### 17.5 When the road route through the stops fails

`polyline:osrm` now asks OSRM `/route` in chunks of at most 25 waypoints that
overlap by one (the shared waypoint's point appears once in the line), within 25
s for the whole route and 12 s per request. The success answer is unchanged. A
failure is still `502 osrm_failed`, and now says why:

```json
{"error": {"code": "osrm_failed",
  "message": "OSRM could not route through these stops: there is no road near KOYAMBEDU (stop ab12cd, row 7)",
  "details": {"reason": "no_segment", "osrm_code": "NoSegment",
              "message": "Could not find a matching segment for coordinate 6",
              "leg": null, "from_stop_id": null, "to_stop_id": null,
              "from_sequence": null, "to_sequence": null, "from_name": null, "to_name": null,
              "waypoint": 6, "stop_id": "ab12cd", "sequence": 7, "stop_name": "KOYAMBEDU"}}}
```

| `reason` | when | where |
| --- | --- | --- |
| `no_segment` | OSRM found no road near a waypoint | `waypoint` (0-based, in the whole route), `stop_id`, `sequence`, `stop_name` |
| `no_route` | no road route between two waypoints; the failing chunk is asked leg by leg to find which | `leg` (from waypoint `leg` to `leg + 1`), `from_stop_id`, `to_stop_id`, `from_sequence`, `to_sequence`, `from_name`, `to_name` |
| `timeout` | OSRM did not answer in time | |
| `unreachable` | OSRM could not be reached | |
| `http_error` | any other answer (`osrm_code` is OSRM's `code`, or the HTTP status) | |

A waypoint is a served stop or a shaping marker (`ROUTE CORRECTION`, whose id is
its `marker_id`); jump and hidden stops are not on the bus's path. A route with
fewer than two positioned waypoints is `no_route` with that message. Run against
a local OSRM built from nandi's `chennai.osm.pbf`, 5,489 of chennai_bus's 5,567
routes get a line through their stops and 78 fail, all of them routes with fewer
than two positioned stops.

### 17.6 Config

An optional block in the GIMS dhall, and the password in the secrets dhall:

```dhall
gtfs_gps = Some
  { url = "https://clickhouse.internal:8443"   -- the HTTP interface, not 9000/9440
  , user = "gims_reader"
  , table = Some "atlas_kafka.amnex_direct_data"
  , days = Some 14
  , feeds = Some [ "chennai_bus" ]
  , max_bus_days = None Natural     -- 30
  , page_rows = None Natural        -- 100
  , timeout_seconds = None Natural  -- 55, under a proxy's 60
  },
gtfs_gps_clickhouse_password = secrets.clickhouse_password,
```

Only `url` and `user` are required. Absent (the default, as in the dev dhall),
the endpoint is `503 gps_unavailable` and nothing else changes; so is a feed not
in `feeds`. An invalid block (a table name that is not `db.table`, `days` outside
1-60, a non-http URL) is logged at boot and leaves the feature off.

### 17.7 Schema, dashboard and tests

`db/gtfs_editor/0021_polyline_source_gps.sql` widens the `gtfs_route.polyline_source`
CHECK to `osrm, gps, manual, upload, imported` (`upload` is another branch's; the
union lets the two run in either order). Safe to run twice. **Apply it before an
image with this endpoint serves drafts**: without it, committing a draft that
holds a `gps` line fails on the CHECK.

The dashboard's route editor offers **Route through stops** and **Suggest from
GPS (last 14 days)** side by side. A GPS line shows its evidence under it - "31
runs by 12 buses, 8–21 Sep, 94% of stops on the line, snapped by OSRM" - with a
warning when under 80% of the stops are on it; either failure shows the server's
reason and what to do about it. The line goes into the draft with `polyline_source:
"gps"`, never with the evidence.

Tests: `src/services/clickhouse_reader.rs` (the guard, readonly=2 and basic auth on
the wire, TSV, no password in `Debug`), `src/services/osrm.rs` (chunk plan,
stitching, the detour guard, the encoder against Google's example),
`src/editor/gps_line.rs` (run selection by direction and variant, cutting, gap
and dwell splits, the synthetic fleet, a loop route, the queries passing the
guard); `tests/editor_gps_line_flow.rs`, registered in
`scripts/editor_flow_test.sh`, against a fake ClickHouse and a fake OSRM on
localhost (every statement checked for readonly=2, a SELECT, a LIMIT, a time
bound; partial, none, the cache, 422, 503, 504, commit of a `gps` line, and each
OSRM reason); `dev/ui_smoke.mjs --map-line` against the mock's `/__dev/map-line`
switch. `examples/gps_line_check.rs` runs the real pipeline read-only against the
real cluster for a few routes and writes GeoJSON to look at.
