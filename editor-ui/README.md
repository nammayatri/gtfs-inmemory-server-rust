# GTFS editor dashboard

The ops dashboard for editing feed metadata: stops, route stop lists with their
fare stages, route names, colours and map lines, and stations. Contract:
`docs/gtfs-editor.md` (sections 2 and 3 are the API this calls, section 4 this UI).

Static HTML, CSS and ES modules. There is no build step and no CDN: libraries
are vendored (`vendor/README.md`). GIMS serves this directory at
`/internal/gtfs-editor/ui/`, and every API call is relative (`../`), so the page
works wherever that pair of paths is mounted.

```
index.html          shell: top bar, side panel, map, page area
css/app.css         the one stylesheet
js/config.js        tile URL, default view, API base    <- the only deployment constants
js/api.js           fetch wrapper: cookies, X-Requested-With, error shape, auth events
js/state.js         shared state; UI preferences in localStorage (never secrets)
js/auth.js          sign-in gate: SSO problems, TOTP enrolment (QR), code entry, lockout
js/main.js          boot, top bar, hash router
js/map.js           Leaflet: stops by area, routes, drag a stop, pick a stop, station selection, insets
js/explore.js       search, stop panel, route panel with the fare-stage ladder
js/editors.js       stop, route stop list, route details and map line, station editors
js/drafts.js        the draft being edited, the draft chooser, addChange()
js/review.js        drafts by status, one draft's diff and its actions
js/admin.js         people (roles, access, two-step reset) and history
dev/                development only - never served by GIMS
```

## Screens

- **Sign-in** - explains a missing SSO identity, an unregistered or disabled
  account; sets up an authenticator app (QR plus setup key); takes the 6-digit
  code with clear wrong-code and lockout messages.
- **Map** (`#/`) - search stops and routes; stops load by map area from zoom 15.
- **Stop** (`#/stop/{id}`) - position, routes using it, station and cluster,
  stops within 60 m. Edit (drag the pin or type coordinates, rename), club into
  a station, delete when unused. A station shows its stops; edit or dissolve it.
- **Route** (`#/route/{id}`) - the stop list as a fare-stage ladder, the map line,
  and a toggle to see it with your draft applied. Edit the stop list (insert from
  search or the map, remove, reorder, change type and stage; fare-rule problems are
  marked on the row with a one-click fix), or the name, colour and map line (ask
  the road router for a line, see it dashed, accept it).
- **Drafts** (`#/drafts`, `#/drafts/{id}`) - by status; a draft shows who did what,
  the actions this person may take (or why not), validation, conflicts, and a
  readable diff per change: field before/after with a map inset, stop list rows
  added/removed/moved/changed, station membership.
- **History** (`#/audit`) and **People** (`#/admin`, admins only).

Edits always go into a draft, remembered per person and feed in this browser.

## Develop against the mock

`dev/mock_server.py` implements the contract in memory (standard library only),
seeded from the editor tables of a **local** Postgres:

```sh
python dev/export_sample.py      # once; reads 127.0.0.1:55432/mtc_internal_master into dev/sample.json.gz
python dev/mock_server.py        # http://127.0.0.1:8765/internal/gtfs-editor/ui/
node dev/ui_smoke.mjs            # drives headless Chrome through every flow, screenshots to $TMPDIR/gtfs-editor-shots
```

The mock injects a purple DEV bar (not part of the dashboard) that stands in for
Pomerium: choose who you are - admin, two editors, an approver, a viewer, a user
who has not set up two-step sign-in, or no SSO identity - and it shows that
person's current authenticator code. TOTP, sessions, roles, maker-checker,
validation, row-version conflicts and the feed version bump are real.
