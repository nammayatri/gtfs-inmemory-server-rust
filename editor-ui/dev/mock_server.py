#!/usr/bin/env python3
"""A development stand-in for the GTFS editor API, standard library only.

Serves the dashboard at /internal/gtfs-editor/ui/ and implements the contract in
docs/gtfs-editor.md sections 2-3 in memory, seeded from dev/sample.json.gz
(written by dev/export_sample.py from a LOCAL Postgres). Nothing it does leaves
this process.

What is faked, and only here:

  - Pomerium. The identity comes from a `dev_email` cookie instead of a signed
    X-Pomerium-Jwt-Assertion. The DEV bar (injected into index.html by this
    server only) sets it, so you can switch between an admin, editors, an
    approver, a viewer, a user who has not enrolled TOTP, and no identity.
  - Everything else follows the contract: TOTP is real RFC 6238 (the DEV bar
    shows the current code), sessions are cookies, roles and maker-checker are
    enforced, commits check row versions and bump the feed version.

    python dev/mock_server.py            # http://127.0.0.1:8765/internal/gtfs-editor/ui/
    python dev/mock_server.py --port 9000
"""
import argparse
import base64
import gzip
import hashlib
import hmac
import json
import math
import mimetypes
import re
import secrets
import struct
import threading
import time
import uuid
from datetime import datetime, timedelta, timezone
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, quote, unquote, urlparse

HERE = Path(__file__).resolve().parent
UI_ROOT = HERE.parent
API = "/internal/gtfs-editor"
UI = API + "/ui"
LOCK = threading.RLock()

SERVED_EXCLUDE = {"ROUTE CORRECTION", "JUMP STOP", "HIDDEN STOP"}
STOP_TYPES = {"NEW STOP", "INTERMEDIATE STOP", "JUMP STOP", "ROUTE CORRECTION", "HIDDEN STOP"}
ROLE_RANK = {"viewer": 0, "editor": 1, "approver": 2, "admin": 3}
SESSION_HOURS = 12
DEV_SECRET = "JBSWY3DPEHPK3PXPJBSWY3DPEHPK3PXP"


# ------------------------------------------------------------------ helpers
def now():
    return datetime.now(timezone.utc)


def iso(dt):
    return dt.isoformat().replace("+00:00", "Z") if dt else None


def haversine(a_lat, a_lon, b_lat, b_lon):
    r = 6371000.0
    p1, p2 = math.radians(a_lat), math.radians(b_lat)
    dp, dl = p2 - p1, math.radians(b_lon - a_lon)
    h = math.sin(dp / 2) ** 2 + math.cos(p1) * math.cos(p2) * math.sin(dl / 2) ** 2
    return 2 * r * math.asin(math.sqrt(h))


def totp(secret_b32, step=None):
    key = base64.b32decode(secret_b32.upper() + "=" * (-len(secret_b32) % 8))
    step = int(time.time() // 30) if step is None else step
    mac = hmac.new(key, struct.pack(">Q", step), hashlib.sha1).digest()
    off = mac[-1] & 0x0F
    return f"{(struct.unpack('>I', mac[off:off + 4])[0] & 0x7FFFFFFF) % 1000000:06d}"


def polyline_encode(points):
    out, prev_lat, prev_lon = [], 0, 0
    for lat, lon in points:
        ilat, ilon = round(lat * 1e5), round(lon * 1e5)
        for v in (ilat - prev_lat, ilon - prev_lon):
            v = ~(v << 1) if v < 0 else v << 1
            while v >= 0x20:
                out.append(chr((0x20 | (v & 0x1F)) + 63))
                v >>= 5
            out.append(chr(v + 63))
        prev_lat, prev_lon = ilat, ilon
    return "".join(out)


def polyline_decode(s):
    pts, idx, lat, lon = [], 0, 0, 0
    while idx < len(s):
        for which in (0, 1):
            shift = result = 0
            while True:
                if idx >= len(s):
                    raise ValueError("truncated polyline")
                b = ord(s[idx]) - 63
                idx += 1
                result |= (b & 0x1F) << shift
                shift += 5
                if b < 0x20:
                    break
            d = ~(result >> 1) if result & 1 else result >> 1
            if which == 0:
                lat += d
            else:
                lon += d
        pts.append((lat / 1e5, lon / 1e5))
    return pts


class ApiError(Exception):
    def __init__(self, status, code, message, details=None):
        super().__init__(message)
        self.status, self.code, self.message, self.details = status, code, message, details or {}


# ------------------------------------------------------------------ store
class Store:
    def __init__(self, sample):
        self.feeds = {f["gtfs_id"]: dict(f) for f in sample["feeds"]}
        self.stops = {(s["gtfs_id"], s["stop_id"]): dict(s) for s in sample["stops"]}
        self.routes = {(r["gtfs_id"], r["route_id"]): dict(r) for r in sample["routes"]}
        self.rows = {}
        for r in sample["route_stops"]:
            self.rows.setdefault((r["gtfs_id"], r["route_id"]), []).append(dict(r))
        self.stop_routes = {}
        for (g, rid), rows in self.rows.items():
            for row in rows:
                if row["stop_id"]:
                    self.stop_routes.setdefault((g, row["stop_id"]), []).append((rid, row["sequence"]))
        t = now()
        self.users = {}
        for email, name, role, enrolled in (
                ("admin@nammayatri.in", "Admin (dev)", "admin", True),
                ("editor1@nammayatri.in", "Priya (editor)", "editor", True),
                ("editor2@nammayatri.in", "Karthik (editor)", "editor", True),
                ("approver1@nammayatri.in", "Meena (approver)", "approver", True),
                ("viewer@nammayatri.in", "Viewer (dev)", "viewer", True),
                ("newuser@nammayatri.in", "New user (dev)", "editor", False)):
            uid = str(uuid.uuid4())
            self.users[uid] = {"user_id": uid, "email": email, "display_name": name, "role": role,
                               "status": "active", "totp_enabled": enrolled,
                               "totp_secret": DEV_SECRET if enrolled else None, "pending_secret": None,
                               "totp_last_step": None, "created_at": iso(t), "last_login_at": None}
        self.bootstrap_admins = {"admin@nammayatri.in"}
        self.sessions = {}
        self.failures = {}
        self.change_sets = {}
        self.audit = []
        self.next_change_id = 1

    # ---- indexes
    def _index_route(self, key, old_rows):
        """Re-index one route after a commit replaced its rows."""
        g, rid = key
        for sid in {r["stop_id"] for r in old_rows if r.get("stop_id")}:
            self.stop_routes[(g, sid)] = [e for e in self.stop_routes.get((g, sid), []) if e[0] != rid]
        for row in self.rows.get(key, []):
            if row["stop_id"]:
                self.stop_routes.setdefault((g, row["stop_id"]), []).append((rid, row["sequence"]))

    def user_by_email(self, email):
        return next((u for u in self.users.values() if u["email"].lower() == (email or "").lower()), None)

    def add_audit(self, actor, action, gtfs_id=None, change_set_id=None, detail=None):
        self.audit.append({"audit_id": len(self.audit) + 1, "at": iso(now()),
                           "actor": actor["user_id"] if actor else None,
                           "actor_email": actor["email"] if actor else None, "action": action,
                           "gtfs_id": gtfs_id, "change_set_id": change_set_id, "detail": detail or {}})


def rows_hash(rows):
    canon = [[r["sequence"], r.get("stop_id"), r["stop_type"], r["stage_no"], r["stage_name"],
              r.get("marker_id"), r.get("marker_lat"), r.get("marker_lon")] for r in rows]
    return hashlib.sha256(json.dumps(canon, separators=(",", ":")).encode()).hexdigest()


# ------------------------------------------------------------------ projection & validation
class Projection:
    """The live data with a change set applied, touching only what it changed."""

    def __init__(self, store, gtfs_id, changes, upto=None):
        self.s, self.g = store, gtfs_id
        self.stops, self.routes, self.rows = {}, {}, {}
        for ch in changes:
            if upto is not None and ch["change_id"] == upto:
                break
            self.apply(ch)

    def stop(self, sid):
        if sid in self.stops:
            return self.stops[sid]
        return self.s.stops.get((self.g, sid))

    def route(self, rid):
        if rid in self.routes:
            return self.routes[rid]
        return self.s.routes.get((self.g, rid))

    def route_rows(self, rid):
        if rid in self.rows:
            return self.rows[rid]
        return self.s.rows.get((self.g, rid), [])

    def routes_using(self, sid):
        live = {rid for rid, _ in self.s.stop_routes.get((self.g, sid), [])}
        live -= set(self.rows)
        live |= {rid for rid, rows in self.rows.items() if any(r.get("stop_id") == sid for r in rows)}
        return live

    def children(self, station_id):
        out = {sid for (g, sid), st in self.s.stops.items() if g == self.g and st.get("parent_station") == station_id}
        for sid, st in self.stops.items():
            if st is None:
                out.discard(sid)
            elif st.get("parent_station") == station_id:
                out.add(sid)
            else:
                out.discard(sid)
        return out

    def apply(self, ch):
        e, op, key, after = ch["entity"], ch["op"], ch["entity_key"], ch.get("after") or {}
        if e == "stop":
            if op == "create":
                self.stops[key] = {"gtfs_id": self.g, "stop_id": key, "stop_code": after.get("stop_code") or key,
                                   "name": after.get("name"), "lat": after.get("lat"), "lon": after.get("lon"),
                                   "location_type": 0, "parent_station": None,
                                   "platform_code": after.get("platform_code"), "cluster_id": None,
                                   "row_version": 1, "deleted": False}
            elif op == "update" and self.stop(key):
                st = dict(self.stop(key))
                st.update({k: v for k, v in after.items() if k in STOP_FIELDS})
                self.stops[key] = st
            elif op == "delete" and self.stop(key):
                st = dict(self.stop(key))
                st["deleted"] = True
                self.stops[key] = st
        elif e == "route" and op == "update" and self.route(key):
            rt = dict(self.route(key))
            rt.update({k: v for k, v in after.items() if k in ROUTE_FIELDS})
            self.routes[key] = rt
        elif e == "route_stops" and op == "replace":
            self.rows[key] = [dict(r, sequence=i + 1, gtfs_id=self.g, route_id=key)
                              for i, r in enumerate(after.get("rows", []))]
        elif e == "station":
            if op in ("create", "update"):
                base = self.stop(key) if op == "update" else None
                st = dict(base) if base else {"gtfs_id": self.g, "stop_id": key, "stop_code": key,
                                                "location_type": 1, "parent_station": None,
                                                "platform_code": None, "cluster_id": None,
                                                "row_version": 1, "deleted": False}
                for k in ("name", "lat", "lon"):
                    if k in after:
                        st[k] = after[k]
                self.stops[key] = st
                if "member_stop_ids" in after:
                    want = set(after["member_stop_ids"])
                    for sid in self.children(key) - want:
                        m = dict(self.stop(sid)); m["parent_station"] = None; self.stops[sid] = m
                    for sid in want:
                        if self.stop(sid):
                            m = dict(self.stop(sid)); m["parent_station"] = key; self.stops[sid] = m
            elif op == "delete" and self.stop(key):
                for sid in self.children(key):
                    m = dict(self.stop(sid)); m["parent_station"] = None; self.stops[sid] = m
                st = dict(self.stop(key)); st["deleted"] = True; self.stops[key] = st


STOP_FIELDS = {"name", "lat", "lon", "platform_code", "cluster_id", "regional_name", "hindi_name"}
ROUTE_FIELDS = {"short_name", "long_name", "color", "encoded_polyline", "polyline_source"}
COLOR_RE = re.compile(r"^#[0-9A-Fa-f]{6}$")


def validate_rows(proj, rows, route_id, change_id):
    out = []

    def err(code, message, **details):
        out.append({"change_id": change_id, "level": "error", "code": code, "message": message,
                    "details": dict(details, route_id=route_id)})

    def name_of(r):
        st = proj.stop(r["stop_id"]) if r.get("stop_id") else None
        return r.get("stop_name_override") or (st or {}).get("name") or r.get("marker_name") or r.get("stop_id") or "?"

    stage_no = stage_name = None
    prev_stop = None
    first_checked = False
    last_new_stage = None
    for i, r in enumerate(rows):
        n = i + 1
        t = r.get("stop_type")
        if t not in STOP_TYPES:
            err("unknown_stop_type", f"Stop {n} has an unknown stop type '{t}'.", row=n)
            continue
        if t == "ROUTE CORRECTION":
            if r.get("marker_lat") is None or r.get("marker_lon") is None:
                err("marker_without_position", f"Row {n} is a map shaping point with no position.", row=n)
            continue
        sid = r.get("stop_id")
        st = proj.stop(sid) if sid else None
        if not sid or not st or st.get("deleted"):
            err("unknown_stop", f"Stop {n} uses stop id {sid or '(empty)'}, which does not exist.", row=n)
        if not first_checked:
            first_checked = True
            if t != "NEW STOP":
                err("first_not_stage_stop",
                    f"The route must start at a fare stage stop. Stop 1 ({name_of(r)}) is marked {t.lower()}.",
                    row=n)
        try:
            no = int(r.get("stage_no"))
        except (TypeError, ValueError):
            err("missing_stage", f"Stop {n} ({name_of(r)}) has no stage number.", row=n)
            continue
        if t == "NEW STOP":
            if last_new_stage is not None and no < last_new_stage:
                err("stage_decreases",
                    f"Stage numbers go back from {last_new_stage} to {no} at stop {n} ({name_of(r)}).", row=n)
            last_new_stage = no
            stage_no, stage_name = no, r.get("stage_name")
        elif t == "INTERMEDIATE STOP" and stage_no is not None:
            if no != stage_no:
                err("intermediate_wrong_stage",
                    f"Stop {n} ({name_of(r)}) is an intermediate stop in stage {stage_no} but carries stage {no}.",
                    row=n, expected_stage_no=stage_no, expected_stage_name=stage_name)
            elif (r.get("stage_name") or "") != (stage_name or ""):
                err("intermediate_wrong_stage_name",
                    f"Stop {n} ({name_of(r)}) is in stage {stage_no}, whose name is '{stage_name}', "
                    f"but it carries the name '{r.get('stage_name')}'.",
                    row=n, expected_stage_no=stage_no, expected_stage_name=stage_name)
        if sid and prev_stop == sid:
            err("repeated_stop", f"Stop {n - 1} and stop {n} are the same stop ({name_of(r)}).", row=n)
        prev_stop = sid
    return out


def validate_set(store, cs):
    g = cs["gtfs_id"]
    results = []
    for ch in cs["changes"]:
        before = Projection(store, g, cs["changes"], upto=ch["change_id"])
        after_proj = Projection(store, g, cs["changes"])
        e, op, key, after = ch["entity"], ch["op"], ch["entity_key"], ch.get("after") or {}

        def err(code, message, level="error", **details):
            results.append({"change_id": ch["change_id"], "level": level, "code": code, "message": message,
                            "details": details})

        if e == "stop":
            if op == "create":
                if before.stop(key):
                    err("stop_id_taken", f"A stop with id {key} already exists.")
                for f in ("name", "lat", "lon"):
                    if after.get(f) in (None, ""):
                        err("missing_field", f"A new stop needs a {f}.")
            elif not before.stop(key):
                err("unknown_stop", f"Stop {key} does not exist.")
            if op in ("create", "update"):
                lat, lon = after.get("lat"), after.get("lon")
                if lat is not None and not (-90 <= float(lat) <= 90):
                    err("bad_position", "Latitude must be between -90 and 90.")
                if lon is not None and not (-180 <= float(lon) <= 180):
                    err("bad_position", "Longitude must be between -180 and 180.")
                old = before.stop(key)
                if op == "update" and old and lat is not None and lon is not None:
                    d = haversine(old["lat"], old["lon"], float(lat), float(lon))
                    if d > 500:
                        err("large_move", f"This moves the stop {d:,.0f} m. Check it is the same place.",
                            level="warning", metres=round(d))
            if op == "delete":
                using = sorted(after_proj.routes_using(key))
                if using:
                    err("stop_in_use", f"Stop {key} is still used by {len(using)} route(s).", routes=using[:20])
        elif e == "route":
            if not before.route(key):
                err("unknown_route", f"Route {key} does not exist.")
            if after.get("color") and not COLOR_RE.match(after["color"]):
                err("bad_color", "Colour must look like #1A7F5A.")
            if after.get("encoded_polyline"):
                try:
                    if len(polyline_decode(after["encoded_polyline"])) < 2:
                        err("bad_polyline", "The polyline has fewer than two points.")
                except ValueError:
                    err("bad_polyline", "The polyline could not be decoded.")
        elif e == "route_stops":
            if not before.route(key):
                err("unknown_route", f"Route {key} does not exist.")
            results.extend(validate_rows(after_proj, after.get("rows", []), key, ch["change_id"]))
        elif e == "station":
            if op == "create" and before.stop(key):
                err("stop_id_taken", f"A stop with id {key} already exists.")
            if op in ("update", "delete"):
                st = before.stop(key)
                if not st or st.get("location_type") != 1:
                    err("unknown_station", f"Station {key} does not exist.")
            if op in ("create", "update"):
                for sid in after.get("member_stop_ids", []):
                    st = before.stop(sid)
                    if not st or st.get("deleted"):
                        err("unknown_stop", f"Member stop {sid} does not exist.")
                    elif st.get("location_type") != 0:
                        err("member_is_station", f"{st['name']} ({sid}) is itself a station.")
                    elif st.get("parent_station") not in (None, key):
                        err("member_has_station",
                            f"{st['name']} ({sid}) already belongs to station {st['parent_station']}.")
    return results


def conflicts_for(store, cs):
    g, out = cs["gtfs_id"], []
    for ch in cs["changes"]:
        key = ch["entity_key"]
        if ch["entity"] in ("stop", "station") and ch["op"] in ("update", "delete") and ch.get("base_row_version"):
            live = store.stops.get((g, key))
            if live and live["row_version"] != ch["base_row_version"]:
                out.append({"change_id": ch["change_id"], "entity": ch["entity"], "entity_key": key,
                            "message": f"Stop {live['name']} ({key}) was changed by a commit after this edit "
                                       f"was made (version {ch['base_row_version']} → {live['row_version']})."})
        elif ch["entity"] == "route" and ch.get("base_row_version"):
            live = store.routes.get((g, key))
            if live and live["row_version"] != ch["base_row_version"]:
                out.append({"change_id": ch["change_id"], "entity": "route", "entity_key": key,
                            "message": f"Route {live.get('short_name')} ({key}) was changed by a commit after "
                                       f"this edit was made."})
        elif ch["entity"] == "route_stops":
            base = (ch.get("after") or {}).get("base_rows_hash")
            if base and base != rows_hash(store.rows.get((g, key), [])):
                rt = store.routes.get((g, key)) or {}
                out.append({"change_id": ch["change_id"], "entity": "route_stops", "entity_key": key,
                            "message": f"The stop list of route {rt.get('short_name')} ({key}) was changed by "
                                       f"a commit after this edit was made."})
    return out


# ------------------------------------------------------------------ serialisers
def stop_out(store, g, st):
    return {k: st.get(k) for k in ("stop_id", "stop_code", "name", "lat", "lon", "location_type",
                                   "parent_station", "platform_code", "cluster_id", "regional_name",
                                   "hindi_name", "position_source", "row_version", "deleted")} | {
        "route_count": len({rid for rid, _ in store.stop_routes.get((g, st["stop_id"]), [])})}


def route_detail(proj, rid):
    rt = proj.route(rid)
    rows = proj.route_rows(rid)
    out_rows = []
    for r in rows:
        st = proj.stop(r["stop_id"]) if r.get("stop_id") else None
        out_rows.append({
            "sequence": r["sequence"], "stop_id": r.get("stop_id"),
            "stop_name": r.get("stop_name_override") or (st or {}).get("name"),
            "lat": (st or {}).get("lat"), "lon": (st or {}).get("lon"),
            "parent_station": (st or {}).get("parent_station"),
            "stop_type": r["stop_type"], "stage_no": r["stage_no"], "stage_name": r["stage_name"],
            "marker_id": r.get("marker_id"), "marker_name": r.get("marker_name"),
            "marker_lat": r.get("marker_lat"), "marker_lon": r.get("marker_lon")})
    return {k: rt.get(k) for k in ("route_id", "short_name", "long_name", "route_type", "color",
                                   "encoded_polyline", "polyline_source", "row_version")} | {
        "rows": out_rows, "rows_hash": rows_hash(rows),
        "stop_count": sum(1 for r in rows if r["stop_type"] not in SERVED_EXCLUDE)}


def user_out(u):
    return {k: u[k] for k in ("user_id", "email", "display_name", "role", "status", "totp_enabled",
                              "created_at", "last_login_at")}


def paginate(items, q):
    limit = min(max(int(q.get("limit", ["50"])[0] or 50), 1), 500)
    start = int(q.get("cursor", ["0"])[0] or 0)
    page = items[start:start + limit]
    nxt = str(start + limit) if start + limit < len(items) else None
    return {"items": page, "next_cursor": nxt}


# ------------------------------------------------------------------ handler
class Handler(BaseHTTPRequestHandler):
    server_version = "gtfs-editor-mock"
    store: Store = None

    def log_message(self, fmt, *args):
        print(f"{self.command} {self.path} -> {args[1] if len(args) > 1 else ''}")

    # ---- plumbing
    def _cookies(self):
        c = SimpleCookie()
        c.load(self.headers.get("Cookie", ""))
        return {k: v.value for k, v in c.items()}

    def _send(self, status, body=None, headers=None, ctype="application/json"):
        data = b""
        if body is not None:
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        if body is not None:
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        for k, v in (headers or []):
            self.send_header(k, v)
        self.end_headers()
        if data and self.command != "HEAD":
            self.wfile.write(data)

    def _body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if not n:
            return {}
        try:
            return json.loads(self.rfile.read(n) or b"{}")
        except json.JSONDecodeError:
            raise ApiError(400, "bad_json", "The request body is not valid JSON.")

    def do_GET(self):
        self._dispatch("GET")

    def do_POST(self):
        self._dispatch("POST")

    def do_PUT(self):
        self._dispatch("PUT")

    def do_PATCH(self):
        self._dispatch("PATCH")

    def do_DELETE(self):
        self._dispatch("DELETE")

    def _dispatch(self, method):
        url = urlparse(self.path)
        path, q = url.path, parse_qs(url.query)
        self.set_cookies = []
        try:
            if path in ("/", API, API + "/", UI):
                return self._send(302, b"", [("Location", UI + "/")], "text/plain")
            if path.startswith(UI + "/"):
                return self._static(path[len(UI) + 1:])
            if path.startswith("/__dev/"):
                return self._dev(method, path, q)
            if not path.startswith(API + "/"):
                raise ApiError(404, "not_found", "No such page.")
            with LOCK:
                status, body = self._api(method, path[len(API):], q)
            headers = [("Set-Cookie", c) for c in self.set_cookies]
            if status == 204:
                return self._send(204, None, headers)
            self._send(status, body, headers)
        except ApiError as e:
            self._send(e.status, {"error": {"code": e.code, "message": e.message, "details": e.details}},
                       [("Set-Cookie", c) for c in self.set_cookies])

    # ---- static files
    def _static(self, rel):
        rel = rel or "index.html"
        if rel.startswith("dev/") and rel != "dev/devbar.js":
            raise ApiError(404, "not_found", "No such file.")
        target = (UI_ROOT / rel).resolve()
        if UI_ROOT not in target.parents or not target.is_file():
            raise ApiError(404, "not_found", "No such file.")
        data = target.read_bytes()
        if rel == "index.html":
            # the DEV bar exists only when served by this mock
            data = data.replace(b"</body>", b'<script type="module" src="dev/devbar.js"></script>\n</body>')
        ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
        if target.suffix in (".js", ".mjs"):
            ctype = "text/javascript"
        self._send(200, data, None, ctype)

    # ---- dev-only endpoints (the DEV bar)
    def _dev(self, method, path, q):
        s = self.store
        if path == "/__dev/state":
            email = unquote(self._cookies().get("dev_email", ""))
            u = s.user_by_email(email)
            secret = (u or {}).get("totp_secret") or (u or {}).get("pending_secret")
            return self._send(200, {"email": email, "users": [user_out(x) for x in s.users.values()],
                                    "current_code": totp(secret) if secret else None})
        if path == "/__dev/as" and method == "POST":
            email = self._body().get("email") or ""
            return self._send(200, {"ok": True}, [
                ("Set-Cookie", f"dev_email={quote(email)}; Path=/; SameSite=Strict"),
                ("Set-Cookie", "gtfs_editor_session=; Path=/; Max-Age=0; SameSite=Strict")])
        raise ApiError(404, "not_found", "No such dev endpoint.")

    # ---- auth
    def identity(self):
        email = unquote(self._cookies().get("dev_email", ""))
        if not email:
            raise ApiError(401, "no_sso_identity",
                           "This page must be opened through the GTFS editor SSO address.")
        u = self.store.user_by_email(email)
        if not u:
            if email in self.store.bootstrap_admins:
                raise ApiError(500, "bootstrap", "bootstrap admin missing")
            raise ApiError(403, "not_registered",
                           f"{email} has not been added to the GTFS editor. Ask an admin to add you.")
        if u["status"] != "active":
            raise ApiError(403, "account_disabled", f"The account {email} is disabled. Ask an admin.")
        return email, u

    def session_user(self):
        email, u = self.identity()
        tok = self._cookies().get("gtfs_editor_session")
        if not tok:
            raise ApiError(401, "session_required", "Sign in with your authenticator code.")
        sess = self.store.sessions.get(hashlib.sha256(tok.encode()).hexdigest())
        if not sess or sess["user_id"] != u["user_id"] or sess["expires_at"] < now():
            raise ApiError(401, "session_required", "Your session has ended. Sign in again.")
        return u

    def require_mutation(self, method):
        if method != "GET" and self.headers.get("X-Requested-With") != "gtfs-editor":
            raise ApiError(403, "missing_request_header", "Mutations need X-Requested-With: gtfs-editor.")

    def require_role(self, u, role):
        if ROLE_RANK[u["role"]] < ROLE_RANK[role]:
            raise ApiError(403, "forbidden", f"This needs the {role} role; your role is {u['role']}.")

    def start_session(self, u):
        tok = secrets.token_urlsafe(32)
        exp = now() + timedelta(hours=SESSION_HOURS)
        self.store.sessions[hashlib.sha256(tok.encode()).hexdigest()] = {"user_id": u["user_id"], "expires_at": exp}
        self.set_cookies.append(f"gtfs_editor_session={tok}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_HOURS * 3600}")
        u["last_login_at"] = iso(now())
        return exp

    def check_code(self, u, secret, code):
        s = self.store
        email = u["email"]
        fails = [t for t in s.failures.get(email, []) if t > time.time() - 600]
        if len(fails) >= 5:
            wait = int(600 - (time.time() - fails[-5]))
            raise ApiError(429, "locked",
                           f"Too many wrong codes. Try again in {max(wait // 60, 1)} minute(s).",
                           {"retry_after_seconds": max(wait, 1)})
        code = re.sub(r"\s", "", str(code or ""))
        step = int(time.time() // 30)
        for st in (step - 1, step, step + 1):
            if hmac.compare_digest(totp(secret, st), code):
                if u["totp_last_step"] is not None and st <= u["totp_last_step"]:
                    raise ApiError(401, "code_reused", "That code was already used. Wait for the next one.")
                u["totp_last_step"] = st
                s.failures.pop(email, None)
                return
        fails.append(time.time())
        s.failures[email] = fails
        left = 5 - len(fails)
        raise ApiError(401, "invalid_code", "That code is not right. Check the time on your phone and try again.",
                       {"attempts_left": left})

    # ---- API
    def _api(self, method, path, q):
        s = self.store
        parts = [unquote(p) for p in path.strip("/").split("/")]

        # auth
        if parts[0] == "auth":
            self.require_mutation(method)
            email, u = self.identity()
            if parts == ["auth", "me"] and method == "GET":
                tok = self._cookies().get("gtfs_editor_session")
                sess = s.sessions.get(hashlib.sha256(tok.encode()).hexdigest()) if tok else None
                valid = bool(sess and sess["user_id"] == u["user_id"] and sess["expires_at"] > now())
                return 200, {"email": u["email"], "display_name": u["display_name"], "role": u["role"],
                             "totp_enabled": u["totp_enabled"], "session": valid}
            if parts == ["auth", "totp", "enroll"] and method == "POST":
                if u["totp_enabled"]:
                    raise ApiError(409, "totp_already_enabled", "Two-step sign-in is already set up.")
                u["pending_secret"] = base64.b32encode(secrets.token_bytes(20)).decode().rstrip("=")
                label = quote(f"GTFS Editor:{u['email']}")
                return 200, {"otpauth_uri": f"otpauth://totp/{label}?secret={u['pending_secret']}"
                                            f"&issuer=GTFS%20Editor&algorithm=SHA1&digits=6&period=30",
                             "secret_base32": u["pending_secret"]}
            if parts == ["auth", "totp", "confirm"] and method == "POST":
                if u["totp_enabled"]:
                    raise ApiError(409, "totp_already_enabled", "Two-step sign-in is already set up.")
                if not u["pending_secret"]:
                    raise ApiError(400, "enroll_first", "Start the set-up again to get a new QR code.")
                self.check_code(u, u["pending_secret"], self._body().get("code"))
                u.update(totp_secret=u["pending_secret"], pending_secret=None, totp_enabled=True)
                exp = self.start_session(u)
                s.add_audit(u, "totp_confirmed")
                return 200, {"totp_enabled": True, "expires_at": iso(exp)}
            if parts == ["auth", "session"] and method == "POST":
                if not u["totp_enabled"]:
                    raise ApiError(400, "enroll_first", "Set up two-step sign-in first.")
                self.check_code(u, u["totp_secret"], self._body().get("code"))
                exp = self.start_session(u)
                s.add_audit(u, "session_created")
                return 200, {"expires_at": iso(exp)}
            if parts == ["auth", "session"] and method == "DELETE":
                tok = self._cookies().get("gtfs_editor_session")
                if tok:
                    s.sessions.pop(hashlib.sha256(tok.encode()).hexdigest(), None)
                self.set_cookies.append("gtfs_editor_session=; Path=/; Max-Age=0; SameSite=Strict")
                return 204, None
            raise ApiError(404, "not_found", "No such auth endpoint.")

        u = self.session_user()
        self.require_mutation(method)

        if parts == ["feeds"] and method == "GET":
            return 200, {"items": [{k: f.get(k) for k in ("gtfs_id", "display_name", "version", "data_source",
                                                          "released_version")} for f in s.feeds.values()],
                         "next_cursor": None}

        if parts[0] == "feeds" and len(parts) >= 2:
            g = parts[1]
            if g not in s.feeds:
                raise ApiError(404, "unknown_feed", f"There is no feed called {g}.")
            rest = parts[2:]
            if rest == ["stops"] and method == "GET":
                return 200, self.list_stops(g, q)
            if len(rest) == 2 and rest[0] == "stops" and method == "GET":
                return 200, self.stop_detail(g, rest[1])
            if rest == ["routes"] and method == "GET":
                return 200, self.list_routes(g, q)
            if len(rest) == 2 and rest[0] == "routes" and method == "GET":
                if (g, rest[1]) not in s.routes:
                    raise ApiError(404, "unknown_route", f"Route {rest[1]} does not exist.")
                return 200, route_detail(Projection(s, g, []), rest[1])
            if len(rest) == 3 and rest[0] == "routes" and rest[2] == "polyline:osrm" and method == "POST":
                self.require_role(u, "editor")
                d = route_detail(Projection(s, g, []), rest[1])
                pts = [(r["lat"], r["lon"]) for r in d["rows"]
                       if r["stop_type"] not in SERVED_EXCLUDE and r["lat"] is not None]
                dist = sum(haversine(*a, *b) for a, b in zip(pts, pts[1:]))
                return 200, {"encoded_polyline": polyline_encode(pts), "polyline_source": "osrm",
                             "distance_m": round(dist), "note": "mock: straight lines between stops"}
            if rest == ["audit"] and method == "GET":
                items = [a for a in reversed(s.audit) if a["gtfs_id"] in (g, None)]
                if q.get("change_set"):
                    items = [a for a in items if a["change_set_id"] == q["change_set"][0]]
                return 200, paginate(items, q)
            if rest == ["change-sets"] and method == "GET":
                items = [cs for cs in s.change_sets.values() if cs["gtfs_id"] == g]
                if q.get("status"):
                    items = [cs for cs in items if cs["status"] in q["status"][0].split(",")]
                items.sort(key=lambda cs: cs["updated_at"], reverse=True)
                return 200, paginate([self.set_summary(cs) for cs in items], q)
            if rest == ["change-sets"] and method == "POST":
                self.require_role(u, "editor")
                b = self._body()
                if not (b.get("title") or "").strip():
                    raise ApiError(400, "missing_title", "Give the draft a title.")
                cs_id, t = str(uuid.uuid4()), iso(now())
                cs = {"change_set_id": cs_id, "gtfs_id": g, "title": b["title"].strip(),
                      "description": b.get("description") or "", "status": "draft",
                      "created_by": u["user_id"], "created_at": t, "updated_at": t,
                      "submitted_by": None, "submitted_at": None, "reviewed_by": None, "reviewed_at": None,
                      "review_comment": None, "committed_by": None, "committed_at": None,
                      "base_version": s.feeds[g]["version"], "committed_version": None, "changes": []}
                s.change_sets[cs_id] = cs
                s.add_audit(u, "change_set_created", g, cs_id, {"title": cs["title"]})
                return 201, self.set_full(cs)
            raise ApiError(404, "not_found", "No such feed endpoint.")

        if parts[0] == "change-sets" and len(parts) >= 2:
            cs = s.change_sets.get(parts[1])
            if not cs:
                raise ApiError(404, "unknown_change_set", "That draft does not exist.")
            rest = parts[2:]
            if not rest and method == "GET":
                return 200, self.set_full(cs)
            if rest == ["changes"] and method == "POST":
                return 201, self.add_change(u, cs, self._body())
            if len(rest) == 2 and rest[0] == "changes" and method in ("PUT", "DELETE"):
                return self.edit_change(u, cs, int(rest[1]), method)
            if len(rest) == 3 and rest[0] == "preview" and rest[1] == "routes" and method == "GET":
                proj = Projection(s, cs["gtfs_id"], cs["changes"])
                if not proj.route(rest[2]):
                    raise ApiError(404, "unknown_route", f"Route {rest[2]} does not exist.")
                return 200, route_detail(proj, rest[2]) | {"validation": validate_set(s, cs),
                                                           "conflicts": conflicts_for(s, cs)}
            if len(rest) == 1 and method == "POST":
                return 200, self.transition(u, cs, rest[0], self._body())
        if parts[0] == "users":
            self.require_role(u, "admin")
            if parts == ["users"] and method == "GET":
                return 200, {"items": [user_out(x) for x in sorted(s.users.values(), key=lambda x: x["email"])],
                             "next_cursor": None}
            if parts == ["users"] and method == "POST":
                b = self._body()
                email = (b.get("email") or "").strip().lower()
                if not re.match(r"^[^@\s]+@[^@\s]+$", email):
                    raise ApiError(400, "bad_email", "Enter a full email address.")
                if s.user_by_email(email):
                    raise ApiError(409, "user_exists", f"{email} is already a user.")
                if b.get("role", "viewer") not in ROLE_RANK:
                    raise ApiError(400, "bad_role", "Unknown role.")
                uid = str(uuid.uuid4())
                s.users[uid] = {"user_id": uid, "email": email, "display_name": b.get("display_name") or None,
                                "role": b.get("role", "viewer"), "status": "active", "totp_enabled": False,
                                "totp_secret": None, "pending_secret": None, "totp_last_step": None,
                                "created_at": iso(now()), "last_login_at": None}
                s.add_audit(u, "user_created", detail={"email": email, "role": s.users[uid]["role"]})
                return 201, user_out(s.users[uid])
            if len(parts) == 2 and method == "PATCH":
                t = s.users.get(parts[1]) or self._404("user")
                b = self._body()
                if t["user_id"] == u["user_id"] and (b.get("role", "admin") != "admin" or b.get("status", "active") != "active"):
                    raise ApiError(403, "cannot_demote_self", "You cannot remove your own admin access.")
                if "role" in b:
                    if b["role"] not in ROLE_RANK:
                        raise ApiError(400, "bad_role", "Unknown role.")
                    t["role"] = b["role"]
                if "status" in b:
                    if b["status"] not in ("active", "disabled"):
                        raise ApiError(400, "bad_status", "Unknown status.")
                    t["status"] = b["status"]
                s.add_audit(u, "user_updated", detail={"email": t["email"], **b})
                return 200, user_out(t)
            if len(parts) == 3 and parts[2] == "reset-totp" and method == "POST":
                t = s.users.get(parts[1]) or self._404("user")
                t.update(totp_enabled=False, totp_secret=None, pending_secret=None, totp_last_step=None)
                for k in [k for k, v in s.sessions.items() if v["user_id"] == t["user_id"]]:
                    s.sessions.pop(k)
                s.add_audit(u, "user_totp_reset", detail={"email": t["email"]})
                return 200, user_out(t)
        raise ApiError(404, "not_found", "No such endpoint.")

    def _404(self, what):
        raise ApiError(404, f"unknown_{what}", f"That {what} does not exist.")

    # ---- reads
    def list_stops(self, g, q):
        s = self.store
        items = [st for (gg, _), st in s.stops.items() if gg == g and not st.get("deleted")]
        if q.get("station", [""])[0] in ("1", "true", "only"):
            items = [st for st in items if st["location_type"] == 1]
        if q.get("bbox"):
            a, b, c, d = (float(x) for x in q["bbox"][0].split(","))
            items = [st for st in items if a <= st["lat"] <= c and b <= st["lon"] <= d]
        term = (q.get("q", [""])[0] or "").strip().lower()
        if term:
            def rank(st):
                if term in (st["stop_id"].lower(), (st.get("stop_code") or "").lower()):
                    return 0
                n = st["name"].lower()
                return 1 if n.startswith(term) else 2 if term in n else 9
            items = sorted((st for st in items if rank(st) < 9), key=lambda st: (rank(st), st["name"]))
        else:
            items.sort(key=lambda st: st["stop_id"])
        page = paginate(items, q)
        page["items"] = [stop_out(s, g, st) for st in page["items"]]
        return page

    def stop_detail(self, g, sid):
        s = self.store
        st = s.stops.get((g, sid))
        if not st:
            raise ApiError(404, "unknown_stop", f"Stop {sid} does not exist.")
        routes = []
        for rid, seq in sorted(s.stop_routes.get((g, sid), []), key=lambda x: (x[0], x[1])):
            rt = s.routes[(g, rid)]
            row = next(r for r in s.rows[(g, rid)] if r["sequence"] == seq)
            routes.append({"route_id": rid, "short_name": rt["short_name"], "long_name": rt["long_name"],
                           "sequence": seq, "stop_type": row["stop_type"], "stage_no": row["stage_no"]})
        children = [stop_out(s, g, c) for (gg, _), c in s.stops.items()
                    if gg == g and c.get("parent_station") == sid and not c.get("deleted")]
        nearby = []
        for (gg, oid), o in s.stops.items():
            if gg != g or oid == sid or o.get("deleted") or abs(o["lat"] - st["lat"]) > 0.001:
                continue
            d = haversine(st["lat"], st["lon"], o["lat"], o["lon"])
            if d <= 60:
                nearby.append(stop_out(s, g, o) | {"distance_m": round(d, 1)})
        nearby.sort(key=lambda x: x["distance_m"])
        parent = s.stops.get((g, st["parent_station"])) if st.get("parent_station") else None
        return stop_out(s, g, st) | {"routes": routes, "children": children, "nearby": nearby,
                                     "parent": stop_out(s, g, parent) if parent else None}

    def list_routes(self, g, q):
        s = self.store
        items = [rt for (gg, _), rt in s.routes.items() if gg == g and not rt.get("deleted")]
        term = (q.get("q", [""])[0] or "").strip().lower()
        if term:
            def rank(rt):
                if term in ((rt.get("short_name") or "").lower(), rt["route_id"].lower()):
                    return 0
                if (rt.get("short_name") or "").lower().startswith(term):
                    return 1
                return 2 if term in (rt.get("long_name") or "").lower() else 9
            items = sorted((rt for rt in items if rank(rt) < 9),
                           key=lambda rt: (rank(rt), rt.get("short_name") or "", rt["route_id"]))
        else:
            items.sort(key=lambda rt: (rt.get("short_name") or "", rt["route_id"]))
        page = paginate(items, q)
        page["items"] = [{"route_id": rt["route_id"], "short_name": rt["short_name"], "long_name": rt["long_name"],
                          "color": rt.get("color"), "has_polyline": bool(rt.get("encoded_polyline")),
                          "stop_count": sum(1 for r in s.rows.get((g, rt["route_id"]), [])
                                            if r["stop_type"] not in SERVED_EXCLUDE),
                          "row_version": rt["row_version"]} for rt in page["items"]]
        return page

    # ---- change sets
    def set_summary(self, cs):
        s = self.store
        name = lambda uid: (s.users.get(uid) or {}).get("email") if uid else None
        return {k: v for k, v in cs.items() if k != "changes"} | {
            "change_count": len(cs["changes"]), "created_by_email": name(cs["created_by"]),
            "submitted_by_email": name(cs["submitted_by"]), "reviewed_by_email": name(cs["reviewed_by"]),
            "committed_by_email": name(cs["committed_by"])}

    def set_full(self, cs):
        s = self.store
        ids = {r.get("stop_id") for ch in cs["changes"] if ch["entity"] == "route_stops"
               for r in (ch.get("after") or {}).get("rows", []) if r.get("stop_id")}
        names = {i: s.stops[(cs["gtfs_id"], i)]["name"] for i in ids if (cs["gtfs_id"], i) in s.stops}
        return self.set_summary(cs) | {"stop_names": names, "changes": cs["changes"], "validation": validate_set(s, cs),
                                       "conflicts": conflicts_for(s, cs),
                                       "feed_version": s.feeds[cs["gtfs_id"]]["version"]}

    def snapshot(self, cs, entity, key):
        proj = Projection(self.store, cs["gtfs_id"], cs["changes"])
        if entity in ("stop", "station"):
            st = proj.stop(key)
            if st is None:
                return None, None
            snap = {k: st.get(k) for k in ("stop_id", "name", "lat", "lon", "platform_code", "cluster_id",
                                           "regional_name", "hindi_name", "location_type", "parent_station")}
            if entity == "station":
                snap["member_stop_ids"] = sorted(proj.children(key))
            live = self.store.stops.get((cs["gtfs_id"], key))
            return snap, (live or {}).get("row_version")
        if entity == "route":
            rt = proj.route(key)
            if rt is None:
                return None, None
            live = self.store.routes.get((cs["gtfs_id"], key))
            return {k: rt.get(k) for k in ROUTE_FIELDS | {"route_id"}}, (live or {}).get("row_version")
        if entity == "route_stops":
            return route_detail(proj, key)["rows"] if proj.route(key) else None, None
        return None, None

    def add_change(self, u, cs, b):
        self.require_role(u, "editor")
        if cs["status"] != "draft":
            raise ApiError(409, "change_set_not_draft", "Only a draft can be edited. Reopen it first.")
        entity, op, key = b.get("entity"), b.get("op"), str(b.get("entity_key") or "").strip()
        valid = {"stop": {"create", "update", "delete"}, "route": {"update"},
                 "route_stops": {"replace"}, "station": {"create", "update", "delete"}}
        if entity not in valid or op not in valid[entity]:
            raise ApiError(400, "bad_change", f"'{op}' on '{entity}' is not a change the editor supports.")
        if not key:
            raise ApiError(400, "bad_change", "entity_key is required.")
        after = b.get("after")
        if op != "delete" and not isinstance(after, dict):
            raise ApiError(400, "bad_change", "after must be an object.")
        if entity == "route_stops":
            if not isinstance(after.get("rows"), list):
                raise ApiError(400, "bad_change", "after.rows must be the whole ordered stop list.")
        before, live_version = self.snapshot(cs, entity, key)
        ch = {"change_id": self.store.next_change_id, "change_set_id": cs["change_set_id"],
              "position": max([c["position"] for c in cs["changes"]] + [0]) + 1,
              "entity": entity, "entity_key": key, "op": op,
              "base_row_version": b.get("base_row_version") or live_version,
              "before": before, "after": after, "created_by": u["user_id"], "created_at": iso(now())}
        self.store.next_change_id += 1
        cs["changes"].append(ch)
        cs["updated_at"] = iso(now())
        self.store.add_audit(u, "change_added", cs["gtfs_id"], cs["change_set_id"],
                             {"entity": entity, "op": op, "entity_key": key})
        return self.set_full(cs)

    def edit_change(self, u, cs, change_id, method):
        self.require_role(u, "editor")
        if cs["status"] != "draft":
            raise ApiError(409, "change_set_not_draft", "Only a draft can be edited. Reopen it first.")
        ch = next((c for c in cs["changes"] if c["change_id"] == change_id), None)
        if not ch:
            raise ApiError(404, "unknown_change", "That change is not in this draft.")
        if method == "DELETE":
            cs["changes"].remove(ch)
            action = "change_removed"
        else:
            b = self._body()
            if ch["op"] != "delete" and not isinstance(b.get("after"), dict):
                raise ApiError(400, "bad_change", "after must be an object.")
            ch["after"] = b.get("after")
            action = "change_updated"
        cs["updated_at"] = iso(now())
        self.store.add_audit(u, action, cs["gtfs_id"], cs["change_set_id"],
                             {"entity": ch["entity"], "entity_key": ch["entity_key"]})
        return 200, self.set_full(cs)

    def transition(self, u, cs, action, b):
        s = self.store
        st = cs["status"]
        t = iso(now())
        if action == "submit":
            self.require_role(u, "editor")
            if st != "draft":
                raise ApiError(409, "bad_status", f"Only a draft can be submitted; this one is {st}.")
            if not cs["changes"]:
                raise ApiError(400, "empty_change_set", "Add at least one change before submitting.")
            errors = [v for v in validate_set(s, cs) if v["level"] == "error"]
            if errors:
                raise ApiError(400, "validation_failed", "Fix the errors before submitting.",
                               {"validation": errors})
            cs.update(status="submitted", submitted_by=u["user_id"], submitted_at=t)
        elif action == "reopen":
            if st not in ("submitted", "rejected", "approved"):
                raise ApiError(409, "bad_status", f"A {st} draft cannot be reopened.")
            if u["user_id"] != cs["created_by"] and u["role"] != "admin":
                raise ApiError(403, "not_author", "Only the person who started this draft, or an admin, can reopen it.")
            cs.update(status="draft", submitted_by=None, submitted_at=None, reviewed_by=None, reviewed_at=None)
        elif action in ("approve", "reject"):
            self.require_role(u, "approver")
            if st != "submitted":
                raise ApiError(409, "bad_status", f"Only a submitted draft can be reviewed; this one is {st}.")
            if u["user_id"] == cs["submitted_by"]:
                raise ApiError(403, "own_submission", "You submitted this change, so someone else must review it.")
            comment = (b.get("comment") or "").strip()
            if action == "reject" and not comment:
                raise ApiError(400, "comment_required", "Say why the change is rejected.")
            cs.update(status="approved" if action == "approve" else "rejected", reviewed_by=u["user_id"],
                      reviewed_at=t, review_comment=comment or None)
        elif action == "commit":
            self.require_role(u, "approver")
            if st != "approved":
                raise ApiError(409, "bad_status", f"Only an approved draft can be committed; this one is {st}.")
            if u["user_id"] == cs["submitted_by"]:
                raise ApiError(403, "own_submission", "You submitted this change, so someone else must commit it.")
            conflicts = conflicts_for(s, cs)
            if conflicts:
                raise ApiError(409, "change_set_conflicts", "Some of this draft's changes were overtaken by another commit.",
                               {"conflicts": conflicts})
            errors = [v for v in validate_set(s, cs) if v["level"] == "error"]
            if errors:
                raise ApiError(400, "validation_failed", "The draft no longer passes validation.",
                               {"validation": errors})
            self.apply_commit(cs)
            s.feeds[cs["gtfs_id"]]["version"] += 1
            cs.update(status="committed", committed_by=u["user_id"], committed_at=t,
                      committed_version=s.feeds[cs["gtfs_id"]]["version"])
        elif action == "discard":
            if st == "committed":
                raise ApiError(409, "bad_status", "A committed draft cannot be discarded.")
            if u["user_id"] != cs["created_by"] and u["role"] != "admin":
                raise ApiError(403, "not_author", "Only the person who started this draft, or an admin, can discard it.")
            cs.update(status="discarded")
        else:
            raise ApiError(404, "not_found", "No such action.")
        cs["updated_at"] = t
        past = {"submit": "submitted", "reopen": "reopened", "approve": "approved", "reject": "rejected",
                "commit": "committed", "discard": "discarded"}[action]
        s.add_audit(u, f"change_set_{past}", cs["gtfs_id"], cs["change_set_id"],
                    {"title": cs["title"], "comment": b.get("comment"), "version": cs.get("committed_version")})
        return self.set_full(cs)

    def apply_commit(self, cs):
        s, g = self.store, cs["gtfs_id"]
        proj = Projection(s, g, cs["changes"])
        for sid, st in proj.stops.items():
            old = s.stops.get((g, sid))
            new = dict(st)
            new["row_version"] = (old["row_version"] + 1) if old else 1
            s.stops[(g, sid)] = new
        for rid, rt in proj.routes.items():
            new = dict(rt)
            new["row_version"] = s.routes[(g, rid)]["row_version"] + 1
            s.routes[(g, rid)] = new
        for rid, rows in proj.rows.items():
            old_rows = s.rows.get((g, rid), [])
            s.rows[(g, rid)] = [dict(r) for r in rows]
            s._index_route((g, rid), old_rows)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--sample", default=str(HERE / "sample.json.gz"))
    args = ap.parse_args()
    if not Path(args.sample).exists():
        raise SystemExit(f"{args.sample} is missing. Run dev/export_sample.py against the local Postgres first.")
    with gzip.open(args.sample, "rt", encoding="utf-8") as fh:
        Handler.store = Store(json.load(fh))
    print(f"GTFS editor mock on http://127.0.0.1:{args.port}{UI}/")
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
