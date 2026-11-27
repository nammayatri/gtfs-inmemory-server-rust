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
js/map.js           Leaflet: the base map (OpenStreetMap or Google), stops by area, routes, several routes at once, drag a stop, pick a stop, station selection, insets, a map line edited by hand
js/line.js          a map line as points: encode it, snap a click to it, replace a stretch, the stops it passes by, and the toolkit's trim, simplify, files in and out (no DOM; tested by dev/line_test.mjs and dev/line_toolkit_test.mjs)
js/routes_map.js    several routes' map lines on the map at once, each in its own colour (#/routes-map?ids=)
js/explore.js       search (paged), stop panel with its CSV download, route panel with the fare-stage ladder
js/editors.js       stop, route stop list, route details and map line (suggested, or edited on the map), station editors
js/drafts.js        the draft being edited, the draft chooser, addChange(), what the draft creates
js/picker.js        choosing a stop: nearby suggestions, search by name or id, or a click on the map
js/create.js        New stop (placed on the map), New route (then its stop list), a stop new in a draft
js/stations.js      stations to review: suggested stations, edit, approve into a draft, reject, reopen
js/merge.js         merging duplicate stops: choose, compare, which id stays, what switches
js/importer.js      bulk import from CSV: templates, reading the file, dry-run check, add to draft
js/csv.js           RFC 4180 CSV reader and writer (no DOM; tested by dev/csv_test.mjs)
js/review.js        drafts by status, one draft's diff (filtered and paged) and its actions
js/admin.js         people (who may work on which feed, admins, two-step reset) and history
js/overlay.js       what the active draft does to a stop, station or route, for display ("pending, not live")
js/context.js       stations listed once instead of their platforms; the "Cleanup context" of a stop and a route
js/history.js       a node's versions (stop, station, route, stop list, stages, stage) and restoring one into a draft
js/undo.js          undo / redo of what is on the screen and not yet in a draft (Ctrl/Cmd+Z)
js/trail.js         the trail of a drill-down under the top bar, with one Back control
dev/                development only - never served by GIMS
```

## Screens

- **Sign-in** - explains a missing SSO identity, an unregistered or disabled
  account; sets up an authenticator app (QR plus setup key); takes the 6-digit
  code with clear wrong-code and lockout messages.
- **Map** (`#/`) - search stops and routes; stops load by map area from zoom 15
  and are named on the map from zoom 17 (one label for a station's platforms,
  and for same-named kerbs within 80 m). Clicking any stop opens it, whatever
  the panel shows; leaving unsaved edits asks first.
- **Stop** (`#/stop/{id}`) - position, routes using it, station and cluster,
  stops within 60 m. Edit (drag the pin or type coordinates, rename), club into
  a station, delete when unused. A station shows its stops; edit or dissolve it.
- **Routes on the map** (`#/routes-map?ids=`) - several routes' map lines at
  once, each in its own colour with its number where it starts: added by
  number or name, from a stop's routes or a route page; picked out by pointing
  at one in the list; their stops on request. The address keeps them.
- **Base map** - the Show box offers OpenStreetMap or Google Maps when the
  server has a Maps JavaScript API key (docs section 30): Google's own map
  under the Leaflet one, following it, its script loaded only once Google is
  chosen. The choice is kept in the browser.
- **Route** (`#/route/{id}`) - the stop list as a fare-stage ladder, the map line,
  and a toggle to see it with your draft applied. Edit the stop list: "Add stop"
  between rows and at the end, "Change stop" on every row (nearby suggestions,
  search by name or id, or pick on the map; a stop is never typed), remove,
  reorder, change type; a stage stop's name is chosen from its own name or the
  route's stage names ("Other name…" only on purpose), and "Renumber stages in
  order" fixes the numbering. Fare-rule problems are marked on the row with a
  one-click fix. Or edit the name, colour and map line: suggest a line through
  the stops or from GPS, or edit it on the map (drag a point, bend it, redraw a
  stretch along the roads or straight), with the stops it passes by more than
  100 m listed as it changes (docs section 28). Its toolkit (section 31) draws
  on from either end by road or straight, routes the line through a stop it
  passes by, snaps it to the roads, trims it to the first and last stop,
  reverses and simplifies it, copies another route's line, and reads and
  writes GeoJSON, GPX and KML.
- **New** (top bar) - New stop (`#/new/stop`: click the map, name, optional
  platform label; the id is made by the server unless typed), New route
  (`#/new/route`: id, number, name, colour, then its stop list; it reaches
  passengers only after the nightly GTFS build gives it trips), New station
  (`#/new/station`), and Import.
- **Stations to review** (`#/stations`, `#/stations/{id}`) - suggested stations by
  status with counts, search and "only in the map area". One suggestion shows its
  point, each stop with its platform label and the routes through it; rename,
  move the point, relabel, drop a stop, then approve into the draft or reject
  with a note. "Approve all in the map area" approves up to 150 at once.
- **Merge** (`#/merge/{stop}`, `?with={other}`) - choose the duplicate (same names
  close by first), compare the two, choose which stop id stays and whose name
  and position to keep, see the routes that switch, add the merge to the draft.
- **Import** (`#/import`) - stops, routes or route stop lists from a CSV file with
  a template per kind; the file is read in the browser, checked by the server
  without changing anything, shown row by row (and stops on a map), and only
  then added to the draft.
- **Drafts** (`#/drafts`, `#/drafts/{id}`) - by status; a draft shows who did what,
  the actions this person may take (or why not), validation, conflicts, and a
  readable diff per change: field before/after with a map inset, stop list rows
  added/removed/moved/changed, station membership and platform labels, merges
  with the routes they update, new routes. Changes are filtered by kind or
  problem and paged 50 at a time, so an import of thousands stays usable.
- **History** (`#/audit`) and **People** (`#/admin`, admins only): one row per
  person, one role picker per feed (none, viewer, editor, approver), an Admin
  switch, system accounts labelled.
- **Feeds** - the top bar offers only the feeds this person holds, with their
  role on the chosen one beside it; every button follows that role. Someone
  with no feed sees "You have no feeds yet — ask an admin" (section 15 of the
  contract).

Edits always go into a draft, remembered per person and feed in this browser.

## Develop against the mock

`dev/mock_server.py` implements the contract in memory (standard library only),
seeded from the editor tables of a **local** Postgres:

```sh
python dev/export_sample.py      # once; reads 127.0.0.1:55432/mtc_internal_master into dev/sample.json.gz
python dev/mock_server.py        # http://127.0.0.1:8765/internal/gtfs-editor/ui/
python dev/mock_server.py --environment master   # plays master's side of a migration (section 23); prod by default
node dev/ui_smoke.mjs            # drives headless Chrome through every flow, screenshots to $TMPDIR/gtfs-editor-shots
node --test dev/csv_test.mjs     # unit tests of the CSV reader
```

The smoke test needs a freshly started mock (it creates drafts and commits).

The mock injects a purple DEV bar (not part of the dashboard) that stands in for
Pomerium: choose who you are - admin, two editors, an approver, a viewer, a user
who has not set up two-step sign-in, a member of a second, small feed, one with
no feed at all, a system account, or no SSO identity - and it shows that
person's current authenticator code. TOTP, sessions, roles, maker-checker,
validation, row-version conflicts and the feed version bump are real, and so
are creating stops and routes (minted ids), bulk import with its dry run, stop
merges, and the station proposal lifecycle (sections 5 and 6 of the contract).
