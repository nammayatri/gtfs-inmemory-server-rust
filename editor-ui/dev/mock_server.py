#!/usr/bin/env python3
"""A development stand-in for the GTFS editor API, standard library only.

Serves the dashboard at /internal/gtfs-editor/ui/ and implements the contract in
docs/gtfs-editor.md in memory: sections 2-3 (access, reads, drafts), 5 (creating
stops and routes, bulk import with a dry run), 6 (station proposals and their
lifecycle) and 8 (coordinate reviews and their lifecycle). It is seeded from
dev/sample.json.gz (written by dev/export_sample.py from a LOCAL Postgres); when
the sample has no coordinate reviews, a few are made up from the stops whose
routes detour most to reach them. Nothing it does leaves this process.

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
import traceback
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

ID_RE = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
COLOR_RE = re.compile(r"^#[0-9A-Fa-f]{6}$")
PLATFORM_MAX = 120
BULK_MAX_ROWS = 5000
BULK_KINDS = ("stops", "routes", "route_stops")
PROPOSAL_STATUSES = ("pending", "approved", "rejected", "committed", "superseded")
PROPOSAL_MOVE_M = 100
REVIEW_STATUSES = ("pending", "approved", "committed", "confirmed", "superseded")
REVIEW_MOVED_M = 25
ID_RULE = "may use up to 64 letters, digits, _ - and . (no spaces or colons)"


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


def median(values):
    vals = sorted(values)
    if not vals:
        return None
    mid = len(vals) // 2
    return vals[mid] if len(vals) % 2 else (vals[mid - 1] + vals[mid]) / 2


def tenth(v):
    """Metres as the API gives them: to a tenth."""
    return None if v is None else round(v * 10) / 10


def leg_detour(call, stop_id, lat, lon):
    """How much further a route travels to call at the point: d(prev, point) +
    d(point, next) - d(prev, next), never below 0; None at either end of the route.
    A neighbour that is the reviewed stop itself moves with it."""
    if not call.get("prev") or not call.get("next"):
        return None
    place = lambda n: (lat, lon) if n["stop_id"] == stop_id else (n["lat"], n["lon"])  # noqa: E731
    (pa, po), (na, no) = place(call["prev"]), place(call["next"])
    return max(0.0, haversine(pa, po, lat, lon) + haversine(lat, lon, na, no) - haversine(pa, po, na, no))


def detour_of(calls, stop_id, lat, lon, only=None):
    """The median detour over the calls with a stop on both sides (of the routes in
    `only`, when given), to a tenth of a metre; None when no call has one (docs 8)."""
    vals = [d for c in calls if only is None or c["route_id"] in only
            for d in [leg_detour(c, stop_id, lat, lon)] if d is not None]
    return tenth(median(vals)) if vals else None


def is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)


def is_int(v):
    return isinstance(v, int) and not isinstance(v, bool)


def valid_position(lat, lon):
    return (is_num(lat) and is_num(lon) and -90 <= lat <= 90 and -180 <= lon <= 180
            and not (lat == 0 and lon == 0))


def norm_name(name):
    return re.sub(r"[^a-z0-9]+", " ", (name or "").lower()).strip()


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


def _safe_header_value(v):
    """A header value carrying CR or LF could smuggle a second header or split
    the response; refuse one outright rather than pass it to send_header, which
    (unlike http.client's request path) does not check for this itself."""
    v = str(v)
    if "\r" in v or "\n" in v:
        raise ValueError("refused to send a header value containing CR/LF")
    return v


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
        self.proposals = {}
        for p in sample.get("station_proposals", []):
            d = dict(p)
            d.update(proposal_id=int(p["proposal_id"]), change_set_id=None, change_id=None, reviewed_by=None,
                     reviewed_at=None, created_at=iso(t), updated_at=iso(t))
            self.proposals[d["proposal_id"]] = d
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
        self.reviews = {}
        self._seed_reviews(sample.get("position_reviews") or [], t)

    # ---- coordinate reviews (docs section 8)
    def _seed_reviews(self, rows, t):
        """The reviews loaded into the local database, or, when it has none, a few
        made up from the stops whose routes detour most to reach them."""
        base = {"change_set_id": None, "change_id": None, "reviewed_by": None, "reviewed_at": None,
                "created_at": iso(t), "updated_at": iso(t)}
        for r in rows:
            d = dict(r, **base)
            d["review_id"] = int(r["review_id"])
            d["evidence"] = r.get("evidence") or {}
            # a move drafted in the database has no draft here: it is waiting again
            if d["status"] == "approved":
                d["status"] = "pending"
            self.reviews[d["review_id"]] = d
        if not self.reviews:
            self._made_up_reviews(base)
        self._mixed_origins_fixture(base)

    # A stop whose routes came from three original stops, shaped like THIRUPORUR
    # THANDALAM (29db2391b0) in the local database: its own routes, plus 515/515A
    # from a Thandalam near Thiruporur and 592/593CT from one near Athupakkam, all
    # put on this kerb by the cleanup (docs section 8.1). Numbers in no group stay
    # "other routes".
    FIXTURE_STOP = "29db2391b0"
    FIXTURE_GROUPS = (
        {"origin_stop_id": "29db2391b0", "origin_name": "THIRUPORUR THANDALAM", "suspect": False,
         "numbers": ("525", "549", "553K", "553W", "554B", "565")},
        {"origin_stop_id": "42b7fdc465", "origin_name": "THANDALAM VILLAGE (THIRUPORUR)", "suspect": True,
         "numbers": ("515", "515A"), "raw": (12.7083, 80.1842),
         "reason": "515 and 515A called at the Thandalam village near Thiruporur, 38 km south; the cleanup moved them "
                   "onto this kerb because the names match."},
        {"origin_stop_id": "7ab6177150", "origin_name": "THANDALAM (ATHUPAKKAM)", "suspect": True,
         "numbers": ("592", "593CT"), "raw": (13.31074, 80.00475),
         "reason": "592 and 593CT run Redhills to Uthukkottai through the Thandalam near Athupakkam, 33 km north."},
    )

    def _mixed_origins_fixture(self, base):
        g = next(iter(self.feeds), None)
        sid = self.FIXTURE_STOP
        st = self.stops.get((g, sid))
        existing = next((r for r in self.reviews.values() if r["gtfs_id"] == g and r["stop_id"] == sid
                         and r["status"] in ("pending", "approved")), None)
        if existing and (existing.get("evidence") or {}).get("route_groups"):
            return
        legs = route_legs(self, g, sid) if st and not st.get("deleted") else []
        if len({x["route_id"] for x in legs}) < 4:
            return
        groups = []
        for spec in self.FIXTURE_GROUPS:
            ids = sorted({x["route_id"] for x in legs if (x["short_name"] or x["route_id"]) in spec["numbers"]})
            if not ids:
                continue
            grp = {"origin_stop_id": spec["origin_stop_id"], "origin_name": spec["origin_name"],
                   "suspect": spec["suspect"], "route_ids": ids,
                   "route_numbers": sorted({x["short_name"] or x["route_id"] for x in legs if x["route_id"] in ids})}
            if spec.get("reason"):
                grp["reason"] = spec["reason"]
            if spec.get("raw"):
                grp["raw_lat"], grp["raw_lon"] = spec["raw"]
            groups.append(grp)
        evidence = {"route_rows": len(self.stop_routes.get((g, sid), [])),
                    "route_numbers": sorted({x["short_name"] or x["route_id"] for x in legs})[:8],
                    "shares_point_with": [], "chalo_nearby": "Thandalam 0m | Thandalam 22m",
                    "map_url": f"https://www.google.com/maps/search/?api=1&query={st['lat']},{st['lon']}",
                    "route_groups": groups, "mixed_origins": len(groups) > 1}
        if existing:
            existing["evidence"] = dict(existing.get("evidence") or {}, **evidence)
            return
        rid = max(self.reviews, default=0) + 1
        self.reviews[rid] = dict(base, review_id=rid, gtfs_id=g, batch="mock-fixture", stop_id=sid, original_stop_id=sid,
                                 stop_name=st["name"], lat=st["lat"], lon=st["lon"], raw_lat=None, raw_lon=None,
                                 suggested_lat=None, suggested_lon=None, suggested_source=None,
                                 reason=("Serves routes from three original stops: its own, 515/515A from a Thandalam near "
                                         "Thiruporur and 592/593CT from a Thandalam near Athupakkam, merged onto this "
                                         "point by the cleanup."),
                                 evidence=evidence, status="pending", review_note=None)

    def _made_up_reviews(self, base):
        g = next(iter(self.feeds), None)
        near, far = [], []
        for (gg, sid), st in self.stops.items():
            if gg != g or st.get("deleted") or st.get("location_type") != 0 or sid == self.FIXTURE_STOP:
                continue
            legs = route_legs(self, g, sid)
            detour = detour_of(legs, sid, st["lat"], st["lon"])
            if detour is not None and detour >= 300 and sum(1 for x in legs if x["prev"] and x["next"]) >= 2:
                (near if detour <= 3000 else far).append((-detour, sid, legs))
        near.sort()
        far.sort()
        # the wrong kerb a few hundred metres off, and the placeholder far away, in turn
        picked = [x for pair in zip(near[:4], far[:4]) for x in pair] or (near + far)[:8]
        for n, (neg, sid, legs) in enumerate(picked, start=1):
            st = self.stops[(g, sid)]
            leg = next(x for x in legs if x["prev"] and x["next"])
            mid = ((leg["prev"]["lat"] + leg["next"]["lat"]) / 2, (leg["prev"]["lon"] + leg["next"]["lon"]) / 2)
            suggestion = mid if n % 3 else None
            raw = (mid[0] + 0.0003, mid[1]) if n % 2 else None
            shares = sorted(({"stop_id": o["stop_id"], "name": o["name"]} for (og, oid), o in self.stops.items()
                             if og == g and oid != sid and not o.get("deleted") and abs(o["lat"] - st["lat"]) < 0.0001
                             and haversine(st["lat"], st["lon"], o["lat"], o["lon"]) <= 2), key=lambda x: x["stop_id"])
            numbers = sorted({x["short_name"] or x["route_id"] for x in legs})
            evidence = {"route_rows": len(self.stop_routes.get((g, sid), [])), "route_numbers": numbers[:8],
                        "shares_point_with": shares, "detour_m": round(-neg),
                        "chalo_nearby": f"{leg['prev']['name'].title()} {round(haversine(st['lat'], st['lon'], leg['prev']['lat'], leg['prev']['lon']))}m" if n % 2 == 0 else "",
                        "map_url": f"https://www.google.com/maps/search/?api=1&query={st['lat']},{st['lon']}"}
            self.reviews[n] = dict(base, review_id=n, gtfs_id=g, batch="mock-made-up", stop_id=sid, original_stop_id=sid,
                                   stop_name=st["name"], lat=st["lat"], lon=st["lon"],
                                   reason=(f"Made up by the mock: its routes go {round(-neg):,} m out of their way to reach it "
                                           f"(between {leg['prev']['name']} and {leg['next']['name']} on route "
                                           f"{leg['short_name'] or leg['route_id']}), so its coordinate may be another place's."),
                                   raw_lat=raw[0] if raw else None, raw_lon=raw[1] if raw else None,
                                   suggested_lat=suggestion[0] if suggestion else None,
                                   suggested_lon=suggestion[1] if suggestion else None,
                                   suggested_source=(f"mock: halfway between {leg['prev']['name']} and {leg['next']['name']}"
                                                     if suggestion else None),
                                   evidence=evidence, status="pending", review_note=None)

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

    def email_of(self, user_id):
        return (self.users.get(user_id) or {}).get("email") if user_id else None

    def default_agency(self, g):
        return next((r.get("agency_id") for (gg, _), r in self.routes.items() if gg == g and r.get("agency_id")), None)

    def add_audit(self, actor, action, gtfs_id=None, change_set_id=None, detail=None):
        self.audit.append({"audit_id": len(self.audit) + 1, "at": iso(now()),
                           "actor": actor["user_id"] if actor else None,
                           "actor_email": actor["email"] if actor else None, "action": action,
                           "gtfs_id": gtfs_id, "change_set_id": change_set_id, "detail": detail or {}})

    def id_in_use(self, g, stop_id):
        if (g, stop_id) in self.stops:
            return True
        return any(ch["entity"] in ("stop", "station") and ch["op"] == "create" and ch["entity_key"] == stop_id
                   for cs in self.change_sets.values() if cs["gtfs_id"] == g and cs["status"] != "discarded"
                   for ch in cs["changes"])

    def mint_stop_id(self, g):
        while True:
            sid = "ed_" + secrets.token_hex(5)
            if not self.id_in_use(g, sid):
                return sid


def rows_hash(rows):
    canon = [[r["sequence"], r.get("stop_id"), r["stop_type"], r["stage_no"], r["stage_name"],
              r.get("marker_id"), r.get("marker_lat"), r.get("marker_lon")] for r in rows]
    return hashlib.sha256(json.dumps(canon, separators=(",", ":")).encode()).hexdigest()


def member_spec(after):
    """The member ids a station change sets, in order, and the platform labels it
    sets ({stop_id: code}), from either `members` or `member_stop_ids`."""
    if isinstance(after.get("members"), list):
        ids, codes = [], {}
        for m in after["members"]:
            if isinstance(m, dict) and m.get("stop_id"):
                ids.append(m["stop_id"])
                if "platform_code" in m:
                    codes[m["stop_id"]] = m["platform_code"]
        return ids, codes
    if isinstance(after.get("member_stop_ids"), list):
        return list(after["member_stop_ids"]), {}
    return None, {}


# ------------------------------------------------------------------ projection & validation
STOP_FIELDS = {"name", "lat", "lon", "platform_code", "cluster_id", "regional_name", "hindi_name"}
ROUTE_FIELDS = {"short_name", "long_name", "color", "text_color", "encoded_polyline", "polyline_source"}


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

    def all_stops(self):
        seen = set()
        for (g, sid), st in self.s.stops.items():
            if g == self.g:
                seen.add(sid)
                yield self.stop(sid)
        for sid, st in self.stops.items():
            if sid not in seen:
                yield st

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
                                   "platform_code": after.get("platform_code"), "cluster_id": after.get("cluster_id"),
                                   "regional_name": after.get("regional_name"), "hindi_name": after.get("hindi_name"),
                                   "position_source": "editor", "row_version": 1, "deleted": False}
            elif op == "update" and self.stop(key):
                st = dict(self.stop(key))
                st.update({k: v for k, v in after.items() if k in STOP_FIELDS})
                self.stops[key] = st
            elif op == "delete" and self.stop(key):
                st = dict(self.stop(key))
                st["deleted"] = True
                self.stops[key] = st
            elif op == "merge" and self.stop(key) and self.stop(after.get("into_stop_id")):
                # every route row on the stop that goes away switches to the one that stays
                into_id = after["into_stop_id"]
                frm, into = dict(self.stop(key)), dict(self.stop(into_id))
                if after.get("keep_name") == "from":
                    into["name"] = frm["name"]
                if after.get("keep_position") == "from":
                    into["lat"], into["lon"] = frm["lat"], frm["lon"]
                self.stops[into_id] = into
                frm.update(deleted=True, merged_into=into_id)
                self.stops[key] = frm
                for rid in self.routes_using(key):
                    self.rows[rid] = [dict(r, stop_id=into_id) if r.get("stop_id") == key else r
                                      for r in self.route_rows(rid)]
        elif e == "route":
            if op == "create":
                self.routes[key] = {"gtfs_id": self.g, "route_id": key, "short_name": after.get("short_name"),
                                    "long_name": after.get("long_name"), "route_type": after.get("route_type") or 3,
                                    "agency_id": after.get("agency_id") or self.s.default_agency(self.g),
                                    "color": after.get("color"), "text_color": None, "encoded_polyline": None,
                                    "polyline_source": None, "service_type": None, "provenance": None,
                                    "row_version": 1, "deleted": False}
            elif op == "update" and self.route(key):
                rt = dict(self.route(key))
                rt.update({k: v for k, v in after.items() if k in ROUTE_FIELDS})
                self.routes[key] = rt
            elif op == "delete" and self.route(key):
                rt = dict(self.route(key))
                rt["deleted"] = True
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
                want, codes = member_spec(after)
                if want is not None:
                    for sid in self.children(key) - set(want):
                        m = dict(self.stop(sid))
                        m["parent_station"] = None
                        self.stops[sid] = m
                    for sid in want:
                        if self.stop(sid):
                            m = dict(self.stop(sid))
                            m["parent_station"] = key
                            if sid in codes:
                                m["platform_code"] = codes[sid]
                            self.stops[sid] = m
            elif op == "delete" and self.stop(key):
                for sid in self.children(key):
                    m = dict(self.stop(sid))
                    m["parent_station"] = None
                    self.stops[sid] = m
                st = dict(self.stop(key))
                st["deleted"] = True
                self.stops[key] = st


def gone(st, sid):
    """(code, message) when a referenced stop no longer exists, else None."""
    if st and st.get("deleted") and st.get("merged_into"):
        return "stop_merged_away", f"{st['name']} ({sid}) is merged into stop {st['merged_into']}."
    if not st or st.get("deleted"):
        return "unknown_stop", f"Stop {sid} does not exist."
    return None


def validate_rows(proj, rows, route_id, change_id):
    """The fare and structure rules of a whole stop list (docs section 3). Each
    finding carries a private `_key` (code and stop) so a problem the live route
    already had can be matched and downgraded."""
    out = []

    def err(code, message, key="", **details):
        out.append({"change_id": change_id, "level": "error", "code": code, "message": message,
                    "details": dict(details, route_id=route_id), "_key": f"{code}|{key}"})

    def name_of(r):
        st = proj.stop(r["stop_id"]) if r.get("stop_id") else None
        return r.get("stop_name_override") or (st or {}).get("name") or r.get("marker_name") or r.get("stop_id") or "?"

    if not rows:
        err("route_empty", "The route has no stops yet. Add at least two stops a passenger can board.")
        return out
    stage_no = stage_name = None
    max_stage = None
    prev_served = None
    first_checked = False
    served = 0
    for i, r in enumerate(rows):
        n = i + 1
        t = r.get("stop_type")
        if t not in STOP_TYPES:
            err("unknown_stop_type", f"Stop {n} has an unknown stop type '{t}'.", key=n, row=n)
            continue
        if not str(r.get("stage_name") or "").strip():
            err("stage_name_missing", f"Stop {n} ({name_of(r)}) has no stage name.",
                key=r.get("stop_id") or r.get("marker_id") or n, row=n)
        if t == "ROUTE CORRECTION":
            if r.get("marker_lat") is None or r.get("marker_lon") is None:
                err("marker_without_position", f"Row {n} is a map shaping point with no position.",
                    key=r.get("marker_id"), row=n)
            continue
        sid = r.get("stop_id")
        st = proj.stop(sid) if sid else None
        if st and st.get("deleted") and st.get("merged_into"):
            err("stop_merged_away", f"Stop {n} uses {st['name']} ({sid}), which is merged into stop {st['merged_into']}.",
                key=sid, row=n)
        elif not sid or not st or st.get("deleted"):
            err("unknown_stop", f"Stop {n} uses stop id {sid or '(empty)'}, which does not exist.", key=sid, row=n)
        elif st.get("location_type") == 1:
            err("stop_is_station", f"Stop {n} uses {st['name']} ({sid}), which is a station. Use one of its stops.",
                key=sid, row=n)
        try:
            no = int(r.get("stage_no"))
        except (TypeError, ValueError):
            err("missing_stage", f"Stop {n} ({name_of(r)}) has no stage number.", key=sid, row=n)
            continue
        if max_stage is not None and no < max_stage:
            err("stage_decreases", f"Stage numbers go back from {max_stage} to {no} at stop {n} ({name_of(r)}).",
                key=f"{sid}|{max_stage}->{no}", row=n)
        max_stage = no if max_stage is None else max(max_stage, no)
        if t == "NEW STOP":
            stage_no, stage_name = no, r.get("stage_name")
        elif t == "INTERMEDIATE STOP":
            if stage_no is None:
                err("intermediate_before_stage", f"Stop {n} ({name_of(r)}) is an intermediate stop before any stage stop.",
                    key=sid, row=n)
            elif no != stage_no:
                err("intermediate_wrong_stage",
                    f"Stop {n} ({name_of(r)}) is an intermediate stop in stage {stage_no} but carries stage {no}.",
                    key=f"{sid}|{no}|{r.get('stage_name')}", row=n, expected_stage_no=stage_no,
                    expected_stage_name=stage_name)
            elif (r.get("stage_name") or "") != (stage_name or ""):
                err("intermediate_wrong_stage_name",
                    f"Stop {n} ({name_of(r)}) is in stage {stage_no}, whose name is '{stage_name}', "
                    f"but it carries the name '{r.get('stage_name')}'.",
                    key=f"{sid}|{no}|{r.get('stage_name')}", row=n, expected_stage_no=stage_no,
                    expected_stage_name=stage_name)
        if t not in SERVED_EXCLUDE:
            served += 1
            if not first_checked:
                first_checked = True
                if t != "NEW STOP":
                    err("first_not_stage_stop",
                        f"The route must start at a fare stage stop. Stop {n} ({name_of(r)}) is marked {t.lower()}.",
                        key=sid, row=n)
            if sid and prev_served == sid:
                err("repeated_stop", f"Stop {n - 1} and stop {n} are the same stop ({name_of(r)}).", key=sid, row=n)
            prev_served = sid
    if served < 2:
        err("too_few_stops", "A route needs at least two stops a passenger can board.")
    return out


def grade(found, live_found):
    """Only problems an edit introduces block it: one the live route already had
    becomes a warning, matched by code and stop, counted."""
    remaining = [f["_key"] for f in live_found if f["level"] == "error"]
    for f in found:
        if f["level"] == "error" and f["_key"] in remaining:
            remaining.remove(f["_key"])
            f["level"] = "warning"
            f["message"] += " (already present before this edit)"
    return found


def validate_change(store, g, ch, before, final, live, route_editors):
    """Findings for one change, against the data as it is just before it (`before`)
    and after the whole set (`final`)."""
    e, op, key, after = ch["entity"], ch["op"], ch["entity_key"], ch.get("after") or {}
    out = []

    def err(code, message, level="error", **details):
        out.append({"change_id": ch["change_id"], "level": level, "code": code, "message": message,
                    "details": details})

    def position(required):
        lat, lon = after.get("lat"), after.get("lon")
        if lat is None and lon is None and not required:
            return
        if not valid_position(lat, lon):
            err("bad_position", "The position must be a latitude between -90 and 90 and a longitude between "
                                "-180 and 180.")

    if e == "stop" and op == "merge":
        into_id = str(after.get("into_stop_id") or "")
        frm, into = before.stop(key), before.stop(into_id)
        problems = [x for x in (gone(frm, key), gone(into, into_id)) if x]
        for code, message in problems:
            err(code, message)
        if problems:
            return out
        kept_name = frm["name"] if after.get("keep_name") == "from" else into["name"]
        for rid in sorted(before.routes_using(key)):
            rows = before.route_rows(rid)
            rt = before.route(rid) or {}
            label = rt.get("short_name") or rid
            label = f"{label} ({rid})" if label != rid else rid
            served = [(r.get("sequence", i + 1), r.get("stop_id")) for i, r in enumerate(rows)
                      if r.get("stop_type") not in SERVED_EXCLUDE]
            repeat = [[sa, sb] for (sa, a), (sb, b) in zip(served, served[1:]) if {a, b} == {key, into_id}]
            if repeat:
                err("merge_would_repeat_stop",
                    f"Route {label} would stop at {kept_name} twice in a row, at stops {repeat[0][0]} and "
                    f"{repeat[0][1]}.", route_id=rid, sequences=[x for pair in repeat for x in pair])
            elif any(r.get("stop_id") == into_id for r in rows):
                seqs = sorted(r.get("sequence", i + 1) for i, r in enumerate(rows) if r.get("stop_id") in (key, into_id))
                err("merge_same_route_twice",
                    f"Route {label} uses both stops (stops {', '.join(map(str, seqs))}), so after the merge it "
                    f"calls at {kept_name} more than once.", level="warning", route_id=rid, sequences=seqs)
        d = haversine(frm["lat"], frm["lon"], into["lat"], into["lon"])
        if d > 150:
            err("merge_far_apart", f"The two stops are {d:,.0f} m apart. Check they are the same place.",
                level="warning", metres=round(d))
        if norm_name(frm["name"]) != norm_name(into["name"]):
            err("merge_names_differ", f"The names differ: {frm['name']} and {into['name']}. The stop that stays is "
                                      f"called {kept_name}.", level="warning")
        return out

    if e == "stop":
        if op == "create":
            if not ID_RE.match(key or ""):
                err("invalid_id", f"The stop id {key!r} {ID_RULE}.")
            if before.stop(key):
                err("stop_id_taken", f"A stop with id {key} already exists.")
            if not str(after.get("name") or "").strip():
                err("missing_field", "A new stop needs a name.")
            position(True)
        else:
            st = before.stop(key)
            if gone(st, key):
                err(*gone(st, key))
            if op == "update":
                position(False)
                prid = after.get("position_review_id")
                if prid is not None:
                    rv = store.reviews.get(prid) if is_int(prid) else None
                    if not rv or rv["gtfs_id"] != g or rv["stop_id"] != key:
                        err("position_review_mismatch",
                            f"Coordinate review #{prid} is not about stop {key}.", review_id=prid)
                old = st
                if old and valid_position(after.get("lat"), after.get("lon")):
                    d = haversine(old["lat"], old["lon"], float(after["lat"]), float(after["lon"]))
                    if d > 500:
                        err("large_move", f"This moves the stop {d:,.0f} m. Check it is the same place.",
                            level="warning", metres=round(d))
            if op == "delete":
                using = sorted(final.routes_using(key))
                if using:
                    err("stop_in_use", f"Stop {key} is still used by {len(using)} route(s).", routes=using[:20])
        pc = after.get("platform_code")
        if isinstance(pc, str) and len(pc) > PLATFORM_MAX:
            err("platform_code_too_long", f"A platform label can be at most {PLATFORM_MAX} characters.")
    elif e == "route":
        rt = before.route(key)
        if op == "create":
            if not ID_RE.match(key or ""):
                err("invalid_id", f"The route id {key!r} {ID_RULE}.")
            if rt:
                err("route_exists", f"A route with id {key} already exists.")
            if not str(after.get("short_name") or "").strip():
                err("missing_field", "A new route needs a route number.")
        elif not rt or rt.get("deleted"):
            err("unknown_route", f"Route {key} does not exist.")
        if op == "delete":
            if route_editors.get(key, set()) - {ch["change_id"]}:
                err("route_has_changes", f"This draft also edits route {key}. Remove those changes before deleting it.")
        if after.get("color") and not COLOR_RE.match(str(after["color"])):
            err("bad_color", "Colour must look like #1A7F5A.")
        if after.get("encoded_polyline"):
            try:
                if len(polyline_decode(after["encoded_polyline"])) < 2:
                    err("bad_polyline", "The polyline has fewer than two points.")
            except ValueError:
                err("bad_polyline", "The polyline could not be decoded.")
    elif e == "route_stops":
        rt = before.route(key)
        if not rt or rt.get("deleted"):
            err("unknown_route", f"Route {key} does not exist.")
        found = validate_rows(before, after.get("rows", []), key, ch["change_id"])
        live_rows = store.rows.get((g, key), [])
        if live_rows:
            found = grade(found, validate_rows(live, live_rows, key, ch["change_id"]))
        out.extend(found)
    elif e == "station":
        if op == "create":
            if not ID_RE.match(key or ""):
                err("invalid_id", f"The station id {key!r} {ID_RULE}.")
            if before.stop(key):
                err("stop_id_taken", f"A stop or station with id {key} already exists.")
            if not str(after.get("name") or "").strip():
                err("missing_field", "A new station needs a name.")
            position(True)
        if op in ("update", "delete"):
            st = before.stop(key)
            if not st or st.get("location_type") != 1 or st.get("deleted"):
                err("unknown_station", f"Station {key} does not exist.")
            if op == "update":
                position(False)
        if op in ("create", "update"):
            want, codes = member_spec(after)
            if op == "create" and want is None:
                err("missing_field", "A new station needs its member stops.")
            for sid in want or []:
                st = before.stop(sid)
                if gone(st, sid):
                    err(*gone(st, sid))
                elif st.get("location_type") != 0:
                    err("member_is_station", f"{st['name']} ({sid}) is itself a station.")
                elif st.get("parent_station") not in (None, key):
                    err("member_has_station",
                        f"{st['name']} ({sid}) already belongs to station {st['parent_station']}.")
            for sid, code in codes.items():
                if code is not None and not isinstance(code, str):
                    err("invalid_payload", f"The platform label of {sid} must be text.")
                elif code and len(code) > PLATFORM_MAX:
                    err("platform_code_too_long",
                        f"The platform label of {sid} is longer than {PLATFORM_MAX} characters.")
    return out


def validate_set(store, cs):
    """Every change's findings. One pass: each change is checked against the data
    just before it, so stops and routes created earlier in the set count."""
    g, changes = cs["gtfs_id"], cs["changes"]
    final = Projection(store, g, changes)
    live = Projection(store, g, [])
    running = Projection(store, g, [])
    route_editors = {}
    for ch in changes:
        if ch["entity"] == "route_stops" or (ch["entity"] == "route" and ch["op"] == "update"):
            route_editors.setdefault(ch["entity_key"], set()).add(ch["change_id"])
    results = []
    for ch in changes:
        results.extend(validate_change(store, g, ch, running, final, live, route_editors))
        running.apply(ch)
    for r in results:
        r.pop("_key", None)
    return results


def conflicts_for(store, cs):
    """Changes whose base no longer matches, checked in order as commit does."""
    g, out = cs["gtfs_id"], []
    running = Projection(store, g, [])
    for ch in cs["changes"]:
        key = ch["entity_key"]
        if ch["entity"] in ("stop", "station") and ch["op"] in ("update", "delete") and ch.get("base_row_version"):
            live = store.stops.get((g, key))
            if live and live["row_version"] != ch["base_row_version"]:
                out.append({"change_id": ch["change_id"], "entity": ch["entity"], "entity_key": key,
                            "reason": "row_version", "expected": ch["base_row_version"],
                            "actual": live["row_version"],
                            "message": f"Stop {live['name']} ({key}) was changed by a commit after this edit "
                                       f"was made (version {ch['base_row_version']} → {live['row_version']})."})
        elif ch["entity"] == "stop" and ch["op"] == "merge":
            after = ch.get("after") or {}
            for sid, base in ((key, ch.get("base_row_version")),
                              (after.get("into_stop_id"), after.get("into_row_version"))):
                live = store.stops.get((g, sid))
                if base and live and live["row_version"] != base:
                    out.append({"change_id": ch["change_id"], "entity": "stop", "entity_key": key,
                                "reason": "row_version", "expected": base, "actual": live["row_version"],
                                "message": f"Stop {live['name']} ({sid}) was changed by a commit after this merge "
                                           f"was made (version {base} → {live['row_version']})."})
        elif ch["entity"] == "route" and ch["op"] in ("update", "delete") and ch.get("base_row_version"):
            live = store.routes.get((g, key))
            if live and live["row_version"] != ch["base_row_version"]:
                out.append({"change_id": ch["change_id"], "entity": "route", "entity_key": key,
                            "reason": "row_version", "expected": ch["base_row_version"],
                            "actual": live["row_version"],
                            "message": f"Route {live.get('short_name')} ({key}) was changed by a commit after "
                                       f"this edit was made."})
        elif ch["entity"] == "route_stops":
            base = (ch.get("after") or {}).get("base_rows_hash")
            actual = rows_hash(running.route_rows(key))
            if base and base != actual:
                rt = running.route(key) or {}
                out.append({"change_id": ch["change_id"], "entity": "route_stops", "entity_key": key,
                            "reason": "rows_hash", "expected": base, "actual": actual,
                            "message": f"The stop list of route {rt.get('short_name')} ({key}) was changed by "
                                       f"a commit after this edit was made."})
        running.apply(ch)
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
            "stop_deleted": (st or {}).get("deleted"),
            "parent_station": (st or {}).get("parent_station"),
            "stop_type": r["stop_type"], "stage_no": r["stage_no"], "stage_name": r["stage_name"],
            "marker_id": r.get("marker_id"), "marker_name": r.get("marker_name"),
            "marker_lat": r.get("marker_lat"), "marker_lon": r.get("marker_lon"),
            "stop_name_override": r.get("stop_name_override"), "provider_id": r.get("provider_id")})
    return {k: rt.get(k) for k in ("route_id", "short_name", "long_name", "route_type", "agency_id", "color",
                                   "text_color", "encoded_polyline", "polyline_source", "provenance",
                                   "deleted", "row_version")} | {
        "rows": out_rows, "rows_hash": rows_hash(rows),
        "stop_count": sum(1 for r in rows if r["stop_type"] not in SERVED_EXCLUDE)}


def route_legs(store, g, sid):
    """Every call a live route makes at the stop, with the served stop before and
    after it on that route (null at either end), from the tables as they are now.
    As the API reads them: a route calling twice is listed twice, a neighbour's name
    is the stop's own, shaping markers and jump and hidden stops are skipped."""
    out = []
    for rid, seq in sorted(store.stop_routes.get((g, sid), []), key=lambda x: (x[0], x[1])):
        rt = store.routes.get((g, rid))
        if not rt or rt.get("deleted"):
            continue
        rows = store.rows.get((g, rid), [])
        at = seq - 1 if 0 < seq <= len(rows) and rows[seq - 1]["sequence"] == seq else \
            next((i for i, r in enumerate(rows) if r["sequence"] == seq), None)
        if at is None:
            continue

        def served(indices):
            for i in indices:
                r = rows[i]
                if r["stop_type"] in SERVED_EXCLUDE or not r.get("stop_id"):
                    continue
                st = store.stops.get((g, r["stop_id"]))
                return {"stop_id": st["stop_id"], "name": st["name"], "lat": st["lat"], "lon": st["lon"]} if st else None
            return None

        out.append({"route_id": rid, "short_name": rt.get("short_name"), "sequence": seq,
                    "stop_type": rows[at]["stop_type"],
                    "prev": served(range(at - 1, -1, -1)), "next": served(range(at + 1, len(rows)))})
    return out


def user_out(u):
    return {k: u[k] for k in ("user_id", "email", "display_name", "role", "status", "totp_enabled",
                              "created_at", "last_login_at")}


def paginate(items, q):
    limit = min(max(int(q.get("limit", ["50"])[0] or 50), 1), 500)
    start = int(q.get("cursor", ["0"])[0] or 0)
    page = items[start:start + limit]
    nxt = str(start + limit) if start + limit < len(items) else None
    return {"items": page, "next_cursor": nxt}


class StopGrid:
    """Stops bucketed in ~110 m cells, for the "same name close by" warning."""
    CELL = 0.001

    def __init__(self, stops=()):
        self.cells = {}
        for st in stops:
            self.add(st)

    def _key(self, lat, lon):
        return int(lat // self.CELL), int(lon // self.CELL)

    def add(self, st):
        self.cells.setdefault(self._key(st["lat"], st["lon"]), []).append(st)

    def same_name_near(self, name, lat, lon, metres):
        want, best = norm_name(name), None
        ky, kx = self._key(lat, lon)
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                for st in self.cells.get((ky + dy, kx + dx), []):
                    if norm_name(st["name"]) != want:
                        continue
                    d = haversine(lat, lon, st["lat"], st["lon"])
                    if d <= metres and (best is None or d < best[0]):
                        best = (d, st)
        return best


# ------------------------------------------------------------------ handler
class Handler(BaseHTTPRequestHandler):
    server_version = "gtfs-editor-mock"
    store: Store = None

    def log_message(self, fmt, *args):
        print(f"{self.command} {self.path[:160]} -> {args[1] if len(args) > 1 else ''}")

    # ---- plumbing
    def _cookies(self):
        c = SimpleCookie()
        c.load(self.headers.get("Cookie", ""))
        return {k: v.value for k, v in c.items()}

    def _send(self, status, body=None, headers=None, ctype="application/json"):
        # Checked before anything is written to the socket: this dev server has
        # no other CRLF check of its own, and several header values below trace
        # back to request input (the static file's guessed content type, cookie
        # values, ...) - a bare "\r\n" in one would smuggle an extra header or
        # split the response.
        ctype = _safe_header_value(ctype)
        headers = [(k, _safe_header_value(v)) for k, v in (headers or [])]
        data = b""
        if body is not None:
            data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        if body is not None:
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(data)))
        self.send_header("Cache-Control", "no-store")
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        if data and self.command != "HEAD":
            self.wfile.write(data)

    def _body(self):
        if hasattr(self, "_cached_body"):
            return self._cached_body
        n = int(self.headers.get("Content-Length") or 0)
        if not n:
            self._cached_body = {}
            return self._cached_body
        try:
            self._cached_body = json.loads(self.rfile.read(n) or b"{}")
        except json.JSONDecodeError:
            raise ApiError(400, "bad_json", "The request body is not valid JSON.")
        if not isinstance(self._cached_body, dict):
            raise ApiError(400, "bad_json", "The request body must be a JSON object.")
        return self._cached_body

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
        except Exception as e:  # a mock bug: say so in the error shape instead of dropping the connection
            traceback.print_exc()
            self._send(500, {"error": {"code": "internal", "message": f"Mock server error: {e}", "details": {}}})

    # ---- static files
    def _static(self, rel):
        rel = rel or "index.html"
        if rel.startswith("dev/") and rel != "dev/devbar.js":
            raise ApiError(404, "not_found", "No such file.")
        target = (UI_ROOT / rel).resolve()
        if not target.is_relative_to(UI_ROOT) or not target.is_file():
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
        if path == "/__dev/supersede-station-proposals" and method == "POST":
            # as on master once stations were made directly: nothing left to review
            with LOCK:
                open_ = [p for p in s.proposals.values() if p["status"] in ("pending", "approved")]
                for p in open_:
                    p.update(status="superseded", change_set_id=None, change_id=None, updated_at=iso(now()))
            return self._send(200, {"superseded": len(open_)})
        if path == "/__dev/context" and method == "POST":
            # round 4 (UX): {"off": true} answers 404 for the context endpoints, as an older server does
            s.context_off = bool(self._body().get("off"))
            return self._send(200, {"off": s.context_off})
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
                return 200, {"user_id": u["user_id"], "email": u["email"], "display_name": u["display_name"],
                             "role": u["role"], "status": u["status"], "totp_enabled": u["totp_enabled"],
                             "session": valid, "sign_in_locked_seconds": 0}
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
            if rest == ["config"] and method == "GET":
                return 200, {"gtfs_id": g, "data_source": s.feeds[g].get("data_source", "preprocessed"),
                             "version": s.feeds[g]["version"]}
            if rest == ["config"] and method == "POST":
                self.require_role(u, "admin")
                to = self._body().get("data_source")
                if to not in ("db", "preprocessed"):
                    raise ApiError(400, "invalid_data_source", "data_source is 'db' or 'preprocessed'.")
                frm = s.feeds[g].get("data_source")
                s.feeds[g]["data_source"] = to
                s.feeds[g]["version"] += 1
                s.add_audit(u, "feed_data_source_changed", g, None, {"gtfs_id": g, "from": frm, "to": to})
                return 200, {"gtfs_id": g, "data_source": to, "version": s.feeds[g]["version"]}
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
                cs = s.change_sets.get(q.get("change_set", [""])[0])
                proj = Projection(s, g, cs["changes"] if cs else [])
                if not proj.route(rest[1]):
                    raise ApiError(404, "unknown_route", f"Route {rest[1]} does not exist.")
                d = route_detail(proj, rest[1])
                pts = [(r["lat"], r["lon"]) for r in d["rows"]
                       if r["stop_type"] not in SERVED_EXCLUDE and r["lat"] is not None]
                dist = sum(haversine(*a, *b) for a, b in zip(pts, pts[1:]))
                return 200, {"route_id": rest[1], "encoded_polyline": polyline_encode(pts),
                             "polyline_source": "osrm", "waypoints": len(pts), "distance_m": round(dist),
                             "saved": False, "note": "mock: straight lines between stops"}
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
            if rest == ["station-proposals"] and method == "GET":
                return 200, self.list_proposals(g, q)
            if rest == ["station-proposals", "summary"] and method == "GET":
                return 200, self.proposal_summary(g)
            if rest == ["station-proposals", "approve"] and method == "POST":
                return 200, self.approve_many(u, g, self._body())
            if rest == ["position-reviews"] and method == "GET":
                return 200, self.list_reviews(g, q)
            if rest == ["position-reviews", "summary"] and method == "GET":
                return 200, self.review_summary(g)
            # round 4 (UX): cleanup context of a stop or a route
            if len(rest) == 3 and rest[0] in ("stops", "routes") and rest[2] == "context" and method == "GET":
                return 200, self.entity_context(g, rest[0], rest[1])
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
                try:
                    change_id = int(rest[1])
                except ValueError:
                    raise ApiError(404, "unknown_change", "That change is not in this draft.")
                return self.edit_change(u, cs, change_id, method)
            if len(rest) == 3 and rest[0] == "preview" and rest[1] == "routes" and method == "GET":
                proj = Projection(s, cs["gtfs_id"], cs["changes"])
                if not proj.route(rest[2]):
                    raise ApiError(404, "unknown_route", f"Route {rest[2]} does not exist.")
                return 200, route_detail(proj, rest[2]) | {"validation": validate_set(s, cs),
                                                           "conflicts": conflicts_for(s, cs)}
            if rest == ["bulk"] and method == "POST":
                return 200, self.bulk(u, cs, self._body())
            if len(rest) == 1 and method == "POST":
                return 200, self.transition(u, cs, rest[0], self._body())

        if parts[0] == "station-proposals" and len(parts) >= 2:
            try:
                p = s.proposals.get(int(parts[1]))
            except ValueError:
                p = None
            if not p:
                raise ApiError(404, "unknown_proposal", "That station proposal does not exist.")
            rest = parts[2:]
            if not rest and method == "GET":
                return 200, self.proposal_detail(p)
            if rest == ["approve"] and method == "POST":
                return 200, self.approve_one(u, p, self._body())
            if rest == ["reject"] and method == "POST":
                return 200, self.reject_proposal(u, p, self._body())
            if rest == ["reopen"] and method == "POST":
                return 200, self.reopen_proposal(u, p)

        if parts[0] == "position-reviews" and len(parts) >= 2:
            try:
                rv = s.reviews.get(int(parts[1]))
            except ValueError:
                rv = None
            if not rv:
                raise ApiError(404, "review_not_found", "That coordinate review does not exist.")
            rest = parts[2:]
            if not rest and method == "GET":
                return 200, self.review_detail(rv, q)
            if rest == ["move"] and method == "POST":
                return 200, self.move_review(u, rv, self._body())
            if rest == ["split"] and method == "POST":
                return 200, self.split_review(u, rv, self._body())
            if rest == ["merge"] and method == "POST":   # round 4 (UX)
                return 200, self.merge_review(u, rv, self._body())
            if rest == ["confirm"] and method == "POST":
                return 200, self.confirm_review(u, rv, self._body())
            if rest == ["reopen"] and method == "POST":
                return 200, self.reopen_review(u, rv)

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
                return 204, None
        raise ApiError(404, "not_found", "No such endpoint.")

    def _404(self, what):
        raise ApiError(404, f"unknown_{what}", f"That {what} does not exist.")

    # ---- reads
    def list_stops(self, g, q):
        s = self.store
        items = [st for (gg, _), st in s.stops.items() if gg == g and not st.get("deleted")]
        station = q.get("station", [""])[0]
        if station in ("1", "true", "only"):
            items = [st for st in items if st["location_type"] == 1]
        elif station:
            items = [st for st in items if st.get("parent_station") == station]
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
        return {k: v for k, v in cs.items() if k != "changes"} | {
            "change_count": len(cs["changes"]), "created_by_email": s.email_of(cs["created_by"]),
            "submitted_by_email": s.email_of(cs["submitted_by"]), "reviewed_by_email": s.email_of(cs["reviewed_by"]),
            "committed_by_email": s.email_of(cs["committed_by"])}

    def set_full(self, cs):
        s = self.store
        closed = cs["status"] in ("committed", "discarded")
        validation = [] if closed else validate_set(s, cs)
        conflicts = [] if closed else conflicts_for(s, cs)
        ids = {r.get("stop_id") for ch in cs["changes"] if ch["entity"] == "route_stops"
               for r in (ch.get("after") or {}).get("rows", []) if r.get("stop_id")}
        names = {i: s.stops[(cs["gtfs_id"], i)]["name"] for i in ids if (cs["gtfs_id"], i) in s.stops}
        return self.set_summary(cs) | {
            "stop_names": names, "changes": cs["changes"], "validation": validation, "conflicts": conflicts,
            "can_submit": (cs["status"] == "draft" and bool(cs["changes"]) and not conflicts
                           and not any(v["level"] == "error" for v in validation)),
            "feed_version": s.feeds[cs["gtfs_id"]]["version"]}

    def snapshot(self, cs, entity, key, op=None, after=None):
        proj = Projection(self.store, cs["gtfs_id"], cs["changes"])
        g = cs["gtfs_id"]
        if entity == "stop" and op == "merge":
            frm, into = proj.stop(key), proj.stop(after["into_stop_id"])
            affected = []
            for rid in proj.routes_using(key):
                rows, rt = proj.route_rows(rid), proj.route(rid) or {}
                affected.append({"route_id": rid, "short_name": rt.get("short_name"), "long_name": rt.get("long_name"),
                                 "sequences": [r["sequence"] for r in rows if r.get("stop_id") == key]})
            affected.sort(key=lambda a: (a["short_name"] or "", a["route_id"]))
            live = self.store.stops.get((g, key))
            return {"from": stop_out(self.store, g, frm), "into": stop_out(self.store, g, into),
                    "affected": affected}, (live or {}).get("row_version")
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
            return {k: rt.get(k) for k in ROUTE_FIELDS | {"route_id", "route_type", "agency_id"}}, \
                (live or {}).get("row_version")
        if entity == "route_stops":
            return route_detail(proj, key)["rows"] if proj.route(key) else None, None
        return None, None

    def append_change(self, u, cs, entity, op, key, after, before=None, base_row_version=None, audit=True):
        ch = {"change_id": self.store.next_change_id, "change_set_id": cs["change_set_id"],
              "position": max([c["position"] for c in cs["changes"]] + [0]) + 1,
              "entity": entity, "entity_key": key, "op": op, "base_row_version": base_row_version,
              "before": before, "after": after, "created_by": u["user_id"], "created_at": iso(now())}
        self.store.next_change_id += 1
        cs["changes"].append(ch)
        cs["updated_at"] = iso(now())
        if audit:
            self.store.add_audit(u, "change_added", cs["gtfs_id"], cs["change_set_id"],
                                 {"change_id": ch["change_id"], "entity": entity, "op": op, "entity_key": key})
        return ch

    def require_draft(self, u, cs):
        self.require_role(u, "editor")
        if cs["status"] != "draft":
            raise ApiError(409, "change_set_not_draft", "Only a draft can be edited. Reopen it first.")

    def add_change(self, u, cs, b):
        self.require_draft(u, cs)
        s = self.store
        entity, op, key = b.get("entity"), b.get("op"), str(b.get("entity_key") or "").strip()
        valid = {"stop": {"create", "update", "delete", "merge"}, "route": {"create", "update", "delete"},
                 "route_stops": {"replace"}, "station": {"create", "update", "delete"}}
        if entity not in valid or op not in valid[entity]:
            raise ApiError(400, "bad_change", f"'{op}' on '{entity}' is not a change the editor supports.")
        after = b.get("after")
        if op != "delete" and not isinstance(after, dict):
            raise ApiError(400, "bad_change", "after must be an object.")
        if op == "delete" and after is not None:
            raise ApiError(400, "bad_change", "A deletion's after must be null.")
        if op == "create":
            id_field = {"stop": "stop_id", "route": "route_id", "station": "station_id"}[entity]
            given = str(after.get(id_field) or "").strip()
            if given and key and given != key:
                raise ApiError(400, "bad_change", f"entity_key must equal {id_field}.")
            key = given or key
            if not key and entity == "stop":
                key = s.mint_stop_id(cs["gtfs_id"])
            if not key:
                raise ApiError(400, "bad_change", f"{id_field} is required.")
            after = dict(after, **{id_field: key})
        if not key:
            raise ApiError(400, "bad_change", "entity_key is required.")
        if entity == "route_stops" and not isinstance(after.get("rows"), list):
            raise ApiError(400, "bad_change", "after.rows must be the whole ordered stop list.")
        if entity == "station" and op in ("create", "update"):
            want, _ = member_spec(after)
            if want is not None and len(want) < 2:
                raise ApiError(400, "invalid_change",
                               f"station/{op}: a station groups at least two stops, and {key} would have "
                               f"{'only one' if want else 'none'}", {"code": "too_few_members"})
        if entity == "stop" and op == "update" and after.get("position_review_id") is not None:
            prid = after["position_review_id"]
            if not is_int(prid) or prid <= 0:
                raise ApiError(400, "invalid_change", "stop/update: position_review_id must be a positive whole number",
                               {"code": "invalid_payload"})
            if "lat" not in after or "lon" not in after:
                raise ApiError(400, "invalid_change",
                               "stop/update: a change for a position review moves the stop (lat and lon)",
                               {"code": "invalid_payload"})
        if entity == "stop" and op == "merge":
            into_id = str(after.get("into_stop_id") or "").strip()
            if not into_id:
                raise ApiError(400, "bad_change", "after.into_stop_id is required: the stop that stays.")
            if into_id == key:
                raise ApiError(400, "bad_change", "A stop cannot be merged into itself.")
            for k in ("keep_name", "keep_position"):
                if after.get(k, "into") not in ("into", "from"):
                    raise ApiError(400, "bad_change", f"{k} must be into or from.")
            proj = Projection(s, cs["gtfs_id"], cs["changes"])
            for sid in (key, into_id):
                st = proj.stop(sid)
                if not st or st.get("deleted"):
                    raise ApiError(404, "entity_not_found", f"There is no stop {sid}.")
                if st.get("location_type") == 1:
                    raise ApiError(400, "merge_station", f"{st['name']} ({sid}) is a station. Merge its stops instead.")
            after = dict(after, into_stop_id=into_id, keep_name=after.get("keep_name", "into"),
                         keep_position=after.get("keep_position", "into"))
        if entity == "route" and op == "delete":
            others = [c for c in cs["changes"] if c["entity_key"] == key and
                      (c["entity"] == "route_stops" or (c["entity"] == "route" and c["op"] != "create"))]
            if others:
                raise ApiError(409, "route_has_changes",
                               f"This draft also edits route {key}. Remove those changes before deleting it.")
        before, live_version = self.snapshot(cs, entity, key, op, after)
        if op != "create" and before is None and entity != "route_stops":
            raise ApiError(404, "entity_not_found", f"There is no {entity} {key}.")
        ch = self.append_change(u, cs, entity, op, key, after, before,
                                b.get("base_row_version") or live_version)
        return self.set_full(cs) | {"change_id": ch["change_id"]}

    def edit_change(self, u, cs, change_id, method):
        self.require_draft(u, cs)
        ch = next((c for c in cs["changes"] if c["change_id"] == change_id), None)
        if not ch:
            raise ApiError(404, "unknown_change", "That change is not in this draft.")
        if method == "DELETE":
            cs["changes"].remove(ch)
            action = "change_removed"
            self.return_proposals(u, cs, "change_removed", change_id=change_id)
            self.review_change_removed(u, cs, ch)
        else:
            b = self._body()
            if ch["op"] != "delete" and not isinstance(b.get("after"), dict):
                raise ApiError(400, "bad_change", "after must be an object.")
            after = b.get("after")
            if ch["op"] == "create":   # the id of something created is fixed once it is in a draft
                id_field = {"stop": "stop_id", "route": "route_id", "station": "station_id"}[ch["entity"]]
                after = dict(after, **{id_field: ch["entity_key"]})
                if ch["entity"] == "station" and (ch.get("after") or {}).get("proposal_id"):
                    after.setdefault("proposal_id", ch["after"]["proposal_id"])
            prid = (ch.get("after") or {}).get("position_review_id")
            if prid is not None and isinstance(after, dict):
                if after.get("position_review_id") is None:
                    after = dict(after, position_review_id=prid)
                elif after["position_review_id"] != prid:
                    raise ApiError(400, "invalid_change", f"this change was made for position review {prid}",
                                   {"code": "position_review_mismatch"})
            ch["after"] = after
            if b.get("base_row_version"):
                ch["base_row_version"] = b["base_row_version"]
            action = "change_updated"
        cs["updated_at"] = iso(now())
        self.store.add_audit(u, action, cs["gtfs_id"], cs["change_set_id"],
                             {"change_id": change_id, "entity": ch["entity"], "entity_key": ch["entity_key"]})
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
            conflicts = conflicts_for(s, cs)
            if conflicts:
                raise ApiError(409, "change_set_conflicts", "Some of this draft's changes were overtaken by another commit.",
                               {"conflicts": conflicts})
            cs.update(status="submitted", submitted_by=u["user_id"], submitted_at=t)
        elif action == "reopen":
            if st not in ("submitted", "rejected", "approved"):
                raise ApiError(409, "bad_status", f"A {st} draft cannot be reopened.")
            if u["user_id"] not in (cs["created_by"], cs["submitted_by"]) and u["role"] != "admin":
                raise ApiError(403, "not_author",
                               "Only the person who started or submitted this draft, or an admin, can reopen it.")
            cs.update(status="draft", submitted_by=None, submitted_at=None, reviewed_by=None, reviewed_at=None)
        elif action in ("approve", "reject"):
            self.require_role(u, "approver")
            if st != "submitted":
                raise ApiError(409, "bad_status", f"Only a submitted draft can be reviewed; this one is {st}.")
            if u["user_id"] == cs["submitted_by"]:
                raise ApiError(403, "own_change_set", "You submitted this change, so someone else must review it.")
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
                raise ApiError(403, "own_change_set", "You submitted this change, so someone else must commit it.")
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
            for p in s.proposals.values():
                if p["change_set_id"] == cs["change_set_id"] and p["status"] == "approved":
                    p.update(status="committed", updated_at=t)
                    s.add_audit(u, "station_proposal_committed", p["gtfs_id"], cs["change_set_id"],
                                {"proposal_id": p["proposal_id"], "station_id": p["station_id"],
                                 "change_id": p["change_id"], "feed_version": cs["committed_version"]})
            for rv in s.reviews.values():
                if rv["change_set_id"] == cs["change_set_id"] and rv["status"] == "approved":
                    rv.update(status="committed", updated_at=t)
                    s.add_audit(u, "position_review_committed", rv["gtfs_id"], cs["change_set_id"],
                                {"review_id": rv["review_id"], "stop_id": rv["stop_id"], "change_id": rv["change_id"],
                                 "feed_version": cs["committed_version"]})
            for ch in cs["changes"]:
                if ch["entity"] == "stop" and ch["op"] == "merge":
                    affected = (ch.get("before") or {}).get("affected") or []
                    s.add_audit(u, "stop_merged", cs["gtfs_id"], cs["change_set_id"], {
                        "change_id": ch["change_id"], "from": ch["entity_key"], "into": ch["after"]["into_stop_id"],
                        "routes": len(affected), "rows": sum(len(r.get("sequences") or []) for r in affected),
                        "keep_name": ch["after"].get("keep_name", "into"),
                        "keep_position": ch["after"].get("keep_position", "into")})
        elif action == "discard":
            if st in ("committed", "discarded"):
                raise ApiError(409, "bad_status", f"A {st} draft cannot be discarded.")
            if u["user_id"] != cs["created_by"] and u["role"] != "admin":
                raise ApiError(403, "not_author", "Only the person who started this draft, or an admin, can discard it.")
            cs.update(status="discarded")
            self.return_proposals(u, cs, "change_set_discarded")
            self.return_reviews(u, cs, "change_set_discarded")
        else:
            raise ApiError(404, "not_found", "No such action.")
        cs["updated_at"] = t
        past = {"submit": "submitted", "reopen": "reopened", "approve": "approved", "reject": "rejected",
                "commit": "committed", "discard": "discarded"}[action]
        detail = {"title": cs["title"], "comment": b.get("comment")}
        if action == "commit":
            detail.update(feed_version=cs["committed_version"], changes=len(cs["changes"]), applied=len(cs["changes"]))
        s.add_audit(u, f"change_set_{past}", cs["gtfs_id"], cs["change_set_id"], detail)
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
            old = s.routes.get((g, rid))
            new = dict(rt)
            new["row_version"] = (old["row_version"] + 1) if old else 1
            s.routes[(g, rid)] = new
        for rid, rows in proj.rows.items():
            old_rows = s.rows.get((g, rid), [])
            s.rows[(g, rid)] = [dict(r) for r in rows]
            s._index_route((g, rid), old_rows)

    # ---- bulk import (docs section 5)
    def bulk(self, u, cs, b):
        s = self.store
        self.require_draft(u, cs)
        kind, rows, dry = b.get("kind"), b.get("rows"), b.get("dry_run", True)
        if kind not in BULK_KINDS:
            raise ApiError(400, "bad_kind", "kind must be stops, routes or route_stops.")
        if not isinstance(rows, list) or not rows:
            raise ApiError(400, "no_rows", "The file has no rows to import.")
        if len(rows) > BULK_MAX_ROWS:
            raise ApiError(400, "too_many_rows",
                           f"At most {BULK_MAX_ROWS:,} rows can be imported at once; this file has {len(rows):,}.",
                           {"max_rows": BULK_MAX_ROWS, "rows": len(rows)})
        if not isinstance(dry, bool):
            raise ApiError(400, "bad_dry_run", "dry_run must be true or false.")
        g = cs["gtfs_id"]
        proj = Projection(s, g, cs["changes"])
        results = [{"row": i + 1, "status": "ok", "messages": [], "change": None} for i in range(len(rows))]

        def msg(i, code, message, level="error"):
            results[i]["messages"].append({"code": code, "message": message, "level": level})

        for i, r in enumerate(rows):
            if not isinstance(r, dict):
                msg(i, "bad_row", "This row is not a set of named columns.")
        changes = {"stops": self.bulk_stops, "routes": self.bulk_routes,
                   "route_stops": self.bulk_route_stops}[kind](g, proj, rows, msg, results)
        for r in results:
            levels = {m["level"] for m in r["messages"]}
            r["status"] = "error" if "error" in levels else "warning" if "warning" in levels else "ok"
        summary = {"rows": len(rows), "ok": sum(r["status"] == "ok" for r in results),
                   "warnings": sum(r["status"] == "warning" for r in results),
                   "errors": sum(r["status"] == "error" for r in results), "changes": len(changes)}
        preview = [{"entity": c["entity"], "op": c["op"], "entity_key": c["entity_key"], "after": c["after"]}
                   for c in changes]
        if dry:
            return {"dry_run": True, "summary": summary, "rows": results, "changes_preview": preview}
        if summary["errors"]:
            raise ApiError(400, "bulk_has_errors",
                           f"{summary['errors']} row(s) have errors. Nothing was added; fix the file and preview again.",
                           {"summary": summary})
        running = Projection(s, g, cs["changes"])
        for c, rows_of in zip(changes, [c.pop("_rows") for c in changes]):
            before = None
            if c["entity"] == "stop" and not c["entity_key"]:
                c["entity_key"] = s.mint_stop_id(g)
                c["after"] = dict(c["after"], stop_id=c["entity_key"])
            if c["entity"] == "route_stops":
                before = route_detail(running, c["entity_key"])["rows"]
            ch = self.append_change(u, cs, c["entity"], c["op"], c["entity_key"], c["after"], before, audit=False)
            running.apply(ch)
            for i in rows_of:
                results[i]["change"] = {"entity": c["entity"], "op": c["op"], "entity_key": c["entity_key"]}
        s.add_audit(u, "bulk_imported", g, cs["change_set_id"],
                    {"kind": kind, "rows": len(rows), "changes": len(changes)})
        preview = [{"entity": c["entity"], "op": c["op"], "entity_key": c["entity_key"], "after": c["after"]}
                   for c in changes]
        return {"dry_run": False, "summary": summary, "rows": results, "changes_preview": preview,
                "change_set": self.set_full(cs)}

    @staticmethod
    def _unknown_columns(r, allowed, what, msg, i):
        for k in r:
            if k not in allowed:
                msg(i, "unknown_field", f"The column {k} is not used when importing {what}.")

    def bulk_stops(self, g, proj, rows, msg, results):
        grid = StopGrid(st for st in proj.all_stops() if st and not st.get("deleted") and st.get("location_type") == 0)
        seen, changes = {}, []
        for i, r in enumerate(rows):
            if not isinstance(r, dict):
                continue
            self._unknown_columns(r, {"stop_id", "name", "lat", "lon", "platform_code"}, "stops", msg, i)
            sid = r.get("stop_id")
            sid = sid.strip() if isinstance(sid, str) else sid
            name = r.get("name").strip() if isinstance(r.get("name"), str) else ""
            lat, lon, pc = r.get("lat"), r.get("lon"), r.get("platform_code")
            if sid not in (None, ""):
                if not isinstance(sid, str) or not ID_RE.match(sid):
                    msg(i, "invalid_id", f"The stop id {sid!r} {ID_RULE}.")
                elif proj.stop(sid):
                    msg(i, "stop_id_taken", f"A stop with id {sid} already exists.")
                elif sid in seen:
                    msg(i, "duplicate_in_upload", f"Row {seen[sid]} of this file has the same stop id.")
                else:
                    seen[sid] = i + 1
            else:
                sid = None
            if not name:
                msg(i, "missing_field", "A new stop needs a name.")
            ok_pos = valid_position(lat, lon)
            if not ok_pos:
                msg(i, "bad_position", "lat and lon must be numbers in range, for example 13.0827 and 80.2707.")
            if pc is not None and not isinstance(pc, str):
                msg(i, "invalid_payload", "platform_code must be text.")
            elif pc and len(pc) > PLATFORM_MAX:
                msg(i, "platform_code_too_long", f"A platform label can be at most {PLATFORM_MAX} characters.")
            if name and ok_pos:
                near = grid.same_name_near(name, lat, lon, 30)
                if near:
                    d, other = near
                    where = other["stop_id"] if not other.get("_row") else f"row {other['_row']} of this file"
                    msg(i, "possible_duplicate",
                        f"{other['name']} ({where}) is {d:.0f} m away. Check this is not the same stop.", "warning")
                grid.add({"stop_id": sid, "name": name, "lat": lat, "lon": lon, "_row": i + 1})
            after = {"name": name, "lat": lat, "lon": lon}
            if sid:
                after = {"stop_id": sid} | after
            if pc:
                after["platform_code"] = pc
            change = {"entity": "stop", "op": "create", "entity_key": sid, "after": after, "_rows": [i]}
            changes.append(change)
            results[i]["change"] = {"entity": "stop", "op": "create", "entity_key": sid}
        return changes

    def bulk_routes(self, g, proj, rows, msg, results):
        seen, changes = {}, []
        for i, r in enumerate(rows):
            if not isinstance(r, dict):
                continue
            self._unknown_columns(r, {"route_id", "short_name", "long_name", "color"}, "routes", msg, i)
            rid = r.get("route_id").strip() if isinstance(r.get("route_id"), str) else r.get("route_id")
            short = r.get("short_name").strip() if isinstance(r.get("short_name"), str) else ""
            color = r.get("color")
            if not isinstance(rid, str) or not rid:
                msg(i, "missing_field", "A new route needs a route id.")
                rid = None
            elif not ID_RE.match(rid):
                msg(i, "invalid_id", f"The route id {rid!r} {ID_RULE}.")
            elif proj.route(rid):
                msg(i, "route_exists", f"A route with id {rid} already exists.")
            elif rid in seen:
                msg(i, "duplicate_in_upload", f"Row {seen[rid]} of this file has the same route id.")
            else:
                seen[rid] = i + 1
            if not short:
                msg(i, "missing_field", "A new route needs a route number (short_name).")
            if color not in (None, "") and not (isinstance(color, str) and COLOR_RE.match(color)):
                msg(i, "bad_color", "Colour must look like #1A7F5A.")
            after = {"route_id": rid, "short_name": short}
            if isinstance(r.get("long_name"), str) and r["long_name"].strip():
                after["long_name"] = r["long_name"].strip()
            if color:
                after["color"] = color
            changes.append({"entity": "route", "op": "create", "entity_key": rid, "after": after, "_rows": [i]})
            results[i]["change"] = {"entity": "route", "op": "create", "entity_key": rid}
        return changes

    def bulk_route_stops(self, g, proj, rows, msg, results):
        s = self.store
        live = Projection(s, g, [])
        by_route = {}
        for i, r in enumerate(rows):
            if not isinstance(r, dict):
                continue
            self._unknown_columns(r, {"route_id", "sequence", "stop_id", "stop_type", "stage_no", "stage_name"},
                                  "route stop lists", msg, i)
            rid = r.get("route_id").strip() if isinstance(r.get("route_id"), str) else None
            if not rid:
                msg(i, "missing_field", "Each row needs the route_id of its route.")
                continue
            rt = proj.route(rid)
            if not rt or rt.get("deleted"):
                msg(i, "unknown_route", f"Route {rid} does not exist, in the feed or in this draft.")
            if not is_int(r.get("sequence")) or r["sequence"] < 1:
                msg(i, "bad_sequence", "sequence must be a whole number from 1.")
            sid = r.get("stop_id").strip() if isinstance(r.get("stop_id"), str) else None
            t = r.get("stop_type")
            if t == "ROUTE CORRECTION":
                msg(i, "marker_not_supported", "Map shaping points (ROUTE CORRECTION) cannot be imported from a file.")
            elif t not in STOP_TYPES:
                msg(i, "unknown_stop_type",
                    f"stop_type must be NEW STOP, INTERMEDIATE STOP, JUMP STOP or HIDDEN STOP, not {t!r}.")
            if not sid:
                msg(i, "missing_field", "Each row needs a stop_id.")
            else:
                st = proj.stop(sid)
                if st and st.get("merged_into"):
                    msg(i, "stop_merged_away", f"Stop {sid} is merged into stop {st['merged_into']}. Use that id.")
                elif not st or st.get("deleted"):
                    msg(i, "unknown_stop", f"Stop {sid} does not exist, in the feed or in this draft.")
                elif st.get("location_type") == 1:
                    msg(i, "stop_is_station", f"{st['name']} ({sid}) is a station. Use one of its stops.")
            if not is_int(r.get("stage_no")):
                msg(i, "missing_stage", "stage_no must be a whole number.")
            if not (isinstance(r.get("stage_name"), str) and r["stage_name"].strip()):
                msg(i, "stage_name_missing", "Each row needs a stage_name.")
            by_route.setdefault(rid, []).append(i)
        changes = []
        skip = {"unknown_stop", "stop_merged_away", "stop_is_station", "unknown_stop_type", "missing_stage",
                "stage_name_missing"}
        for rid, idxs in by_route.items():
            rt = proj.route(rid)
            if not rt or rt.get("deleted"):
                continue
            seqs = {}
            for i in idxs:
                seq = rows[i].get("sequence")
                if is_int(seq):
                    if seq in seqs:
                        msg(i, "duplicate_sequence", f"Row {seqs[seq]} of this file is also sequence {seq} of route {rid}.")
                    else:
                        seqs[seq] = i + 1
            ordered = sorted(idxs, key=lambda i: (rows[i]["sequence"] if is_int(rows[i].get("sequence")) else 10 ** 9, i))
            new_rows = [{"stop_id": rows[i].get("stop_id"), "stop_type": rows[i].get("stop_type"),
                         "stage_no": rows[i].get("stage_no"), "stage_name": rows[i].get("stage_name"),
                         "marker_id": None, "marker_name": None, "marker_lat": None, "marker_lon": None,
                         "stop_name_override": None, "provider_id": None} for i in ordered]
            found = validate_rows(proj, new_rows, rid, None)
            live_rows = s.rows.get((g, rid), [])
            if live_rows:
                found = grade(found, validate_rows(live, live_rows, rid, None))
            for f in found:
                if f["code"] in skip:
                    continue
                pos = f["details"].get("row")
                msg(ordered[pos - 1] if pos else ordered[0], f["code"], f["message"], f["level"])
            current = proj.route_rows(rid)
            markers = sum(1 for x in current if x["stop_type"] == "ROUTE CORRECTION")
            overrides = sum(1 for x in current if x.get("stop_name_override"))
            if markers or overrides:
                parts = [f"{markers} map shaping point{'s' if markers != 1 else ''}"] if markers else []
                if overrides:
                    parts.append(f"{overrides} route-specific stop name{'s' if overrides != 1 else ''}")
                msg(ordered[0], "drops_route_details",
                    f"Route {rid} has {' and '.join(parts)} that this import removes.", "warning")
            if rid in proj.rows:
                msg(ordered[0], "replaces_draft_change",
                    f"This draft already changes the stop list of route {rid}; this import adds a newer version after it.",
                    "warning")
            changes.append({"entity": "route_stops", "op": "replace", "entity_key": rid,
                            "after": {"rows": new_rows, "base_rows_hash": rows_hash(current)}, "_rows": idxs})
            for i in idxs:
                results[i]["change"] = {"entity": "route_stops", "op": "replace", "entity_key": rid}
        return changes

    # ---- station proposals (docs section 6)
    def proposal_out(self, p):
        s = self.store
        cs = s.change_sets.get(p["change_set_id"]) if p.get("change_set_id") else None
        return {k: p.get(k) for k in ("proposal_id", "station_id", "name", "lat", "lon", "spread_m", "members",
                                      "status", "change_set_id", "change_id", "reviewed_at", "review_note",
                                      "batch")} | {
            "change_set_title": cs["title"] if cs else None, "reviewed_by_email": s.email_of(p.get("reviewed_by"))}

    def proposal_detail(self, p):
        s, g = self.store, p["gtfs_id"]
        out = self.proposal_out(p)
        members, problems = [], []
        check = p["status"] == "pending"
        for m in p["members"]:
            st = s.stops.get((g, m["stop_id"]))
            members.append(dict(m, current=stop_out(s, g, st) if st else None))
            if not check:
                continue
            label = f"{m['name']} ({m['stop_id']})"
            if not st:
                problems.append({"code": "member_missing", "stop_id": m["stop_id"], "message": f"{label} no longer exists."})
            elif st.get("deleted"):
                problems.append({"code": "member_deleted", "stop_id": m["stop_id"], "message": f"{label} has been deleted."})
            elif st.get("location_type") != 0:
                problems.append({"code": "member_is_station", "stop_id": m["stop_id"], "message": f"{label} is now a station."})
            else:
                if st.get("parent_station"):
                    problems.append({"code": "member_has_parent", "stop_id": m["stop_id"],
                                     "message": f"{label} now belongs to station {st['parent_station']}."})
                d = haversine(m["lat"], m["lon"], st["lat"], st["lon"])
                if d > PROPOSAL_MOVE_M:
                    problems.append({"code": "member_moved", "stop_id": m["stop_id"],
                                     "message": f"{label} has moved {d:,.0f} m since this station was suggested."})
        if check and (g, p["station_id"]) in s.stops:
            problems.append({"code": "station_id_taken", "message": f"A stop or station with id {p['station_id']} already exists."})
        out["members"] = members
        out["problems"] = problems
        return out

    def proposal_summary(self, g):
        counts = {k: 0 for k in ("pending", "approved", "rejected", "committed")}
        for p in self.store.proposals.values():
            if p["gtfs_id"] == g and p["status"] in counts:
                counts[p["status"]] += 1
        return counts

    def list_proposals(self, g, q):
        s = self.store
        statuses = [x for x in (q.get("status", [""])[0] or "pending").split(",") if x]
        bad = [x for x in statuses if x not in PROPOSAL_STATUSES]
        if bad:
            raise ApiError(400, "bad_status", f"Unknown status {bad[0]}.")
        items = [p for p in s.proposals.values() if p["gtfs_id"] == g and p["status"] in statuses]
        if q.get("bbox"):
            try:
                a, b, c, d = (float(x) for x in q["bbox"][0].split(","))
            except ValueError:
                raise ApiError(400, "bad_bbox", "bbox is minLat,minLon,maxLat,maxLon.")
            items = [p for p in items if a <= p["lat"] <= c and b <= p["lon"] <= d]
        term = (q.get("q", [""])[0] or "").strip().lower()
        if term:
            def rank(p):
                if term == p["station_id"].lower() or any(term == m["stop_id"].lower() for m in p["members"]):
                    return 0
                n = p["name"].lower()
                return 1 if n.startswith(term) else 2 if term in n else 9
            items = sorted((p for p in items if rank(p) < 9), key=lambda p: (rank(p), p["name"].lower(), p["proposal_id"]))
        else:
            items.sort(key=lambda p: p["proposal_id"])
        page = paginate(items, q)
        page["items"] = [self.proposal_out(p) for p in page["items"]]
        return page

    def target_draft(self, u, g, b):
        s = self.store
        self.require_role(u, "editor")
        cs = s.change_sets.get(str(b.get("change_set_id") or ""))
        if not cs:
            raise ApiError(404, "unknown_change_set", "That draft does not exist.")
        if cs["gtfs_id"] != g:
            raise ApiError(400, "wrong_feed", "That draft belongs to another feed.")
        if cs["status"] != "draft":
            raise ApiError(409, "change_set_not_draft", "Only a draft can be edited. Reopen it first.")
        return cs

    def proposal_problems(self, p, proj, name, lat, lon, members):
        problems = []
        if not str(name or "").strip():
            problems.append({"code": "name_missing", "message": "The station needs a name."})
        if not valid_position(lat, lon):
            problems.append({"code": "bad_position", "message": "The station point is not a valid position."})
        if proj.stop(p["station_id"]):
            problems.append({"code": "station_id_taken",
                             "message": f"A stop or station with id {p['station_id']} already exists."})
        if len(members) == 1:
            problems.append({"code": "too_few_members",
                             "message": f"a station groups at least two stops, and {p['station_id']} would have only one"})
        elif not members:
            problems.append({"code": "no_members", "message": f"station {p['station_id']} would have no stops"})
        for m in members:
            sid = m["stop_id"]
            st = proj.stop(sid)
            if not st or st.get("deleted"):
                problems.append({"code": "member_missing", "stop_id": sid, "message": f"Stop {sid} no longer exists."})
            elif st.get("location_type") != 0:
                problems.append({"code": "member_is_station", "stop_id": sid, "message": f"{st['name']} ({sid}) is a station."})
            elif st.get("parent_station"):
                problems.append({"code": "member_has_parent", "stop_id": sid,
                                 "message": f"{st['name']} ({sid}) already belongs to station {st['parent_station']}."})
            code = m.get("platform_code")
            if code is not None and not isinstance(code, str):
                problems.append({"code": "invalid_payload", "stop_id": sid, "message": f"The platform label of {sid} must be text."})
            elif code and len(code) > PLATFORM_MAX:
                problems.append({"code": "platform_code_too_long", "stop_id": sid,
                                 "message": f"The platform label of {sid} is longer than {PLATFORM_MAX} characters."})
        return problems

    def approve_into(self, u, cs, p, proj, name, lat, lon, members):
        after = {"station_id": p["station_id"], "name": name.strip(), "lat": lat, "lon": lon,
                 "members": [{"stop_id": m["stop_id"], "platform_code": m.get("platform_code")} for m in members],
                 "proposal_id": p["proposal_id"]}
        ch = self.append_change(u, cs, "station", "create", p["station_id"], after)
        proj.apply(ch)
        t = iso(now())
        p.update(status="approved", change_set_id=cs["change_set_id"], change_id=ch["change_id"],
                 reviewed_by=u["user_id"], reviewed_at=t, review_note=None, updated_at=t)
        return ch

    def approve_one(self, u, p, b):
        s = self.store
        cs = self.target_draft(u, p["gtfs_id"], b)
        if p["status"] != "pending":
            raise ApiError(409, "proposal_not_pending", f"This proposal is {p['status']}, not waiting for review.")
        name = b.get("name", p["name"])
        lat, lon = b.get("lat", p["lat"]), b.get("lon", p["lon"])
        if ("lat" in b) != ("lon" in b):
            raise ApiError(400, "bad_position", "Send lat and lon together.")
        by_id = {m["stop_id"]: m for m in p["members"]}
        if b.get("members") is None:
            members = [{"stop_id": m["stop_id"], "platform_code": m.get("platform_code")} for m in p["members"]]
        else:
            if not isinstance(b["members"], list):
                raise ApiError(400, "bad_members", "members must be a list of {stop_id, platform_code}.")
            members, seen = [], set()
            for m in b["members"]:
                sid = m.get("stop_id") if isinstance(m, dict) else None
                if sid not in by_id:
                    raise ApiError(400, "member_not_in_proposal",
                                   f"Stop {sid} is not part of this proposal. A reviewer can drop stops, not add them.")
                if sid in seen:
                    raise ApiError(400, "bad_members", f"Stop {sid} is listed twice.")
                seen.add(sid)
                members.append({"stop_id": sid, "platform_code": m.get("platform_code", by_id[sid].get("platform_code"))})
        proj = Projection(s, cs["gtfs_id"], cs["changes"])
        problems = self.proposal_problems(p, proj, name, lat, lon, members)
        if problems:
            raise ApiError(400, "proposal_has_problems", "the station would not be valid now; see the problems",
                           {"problems": problems})
        ch = self.approve_into(u, cs, p, proj, name, lat, lon, members)
        s.add_audit(u, "station_proposal_approved", p["gtfs_id"], cs["change_set_id"],
                    {"proposal_id": p["proposal_id"], "station_id": p["station_id"], "change_id": ch["change_id"],
                     "renamed": name != p["name"], "moved": "lat" in b, "members": len(members)})
        # like the API: the proposal detail, now approved with change_set_id and change_id
        return self.proposal_detail(p)

    def approve_many(self, u, g, b):
        s = self.store
        cs = self.target_draft(u, g, b)
        ids = b.get("proposal_ids")
        if not isinstance(ids, list) or not ids or len(ids) > 500:
            raise ApiError(400, "bad_proposal_ids", "Send between 1 and 500 proposal ids.")
        proj = Projection(s, g, cs["changes"])
        results = []
        for pid in ids:
            p = s.proposals.get(pid) if is_int(pid) else None
            if not p or p["gtfs_id"] != g:
                results.append({"proposal_id": pid, "ok": False,
                                "problems": [{"code": "unknown_proposal", "message": "No such proposal in this feed."}]})
                continue
            if p["status"] != "pending":
                results.append({"proposal_id": pid, "ok": False, "problems": [
                    {"code": "proposal_not_pending", "message": f"{p['name']} is {p['status']}, not waiting for review."}]})
                continue
            members = [{"stop_id": m["stop_id"], "platform_code": m.get("platform_code")} for m in p["members"]]
            problems = self.proposal_problems(p, proj, p["name"], p["lat"], p["lon"], members)
            if problems:
                results.append({"proposal_id": pid, "ok": False, "problems": problems})
                continue
            ch = self.approve_into(u, cs, p, proj, p["name"], p["lat"], p["lon"], members)
            results.append({"proposal_id": pid, "ok": True, "change_id": ch["change_id"]})
            s.add_audit(u, "station_proposal_approved", g, cs["change_set_id"],
                        {"proposal_id": pid, "station_id": p["station_id"], "change_id": ch["change_id"], "bulk": True})
        added = sum(1 for r in results if r["ok"])
        return {"results": results, "approved": added, "skipped": len(results) - added,
                "change_set_id": cs["change_set_id"]}

    def reject_proposal(self, u, p, b):
        self.require_role(u, "editor")
        note = (b.get("note") or "").strip() if isinstance(b.get("note"), str) else ""
        if not note:
            raise ApiError(400, "note_required", "Write why this station is rejected.")
        if p["status"] != "pending":
            raise ApiError(409, "proposal_not_pending", f"This proposal is {p['status']}, not waiting for review.")
        t = iso(now())
        p.update(status="rejected", reviewed_by=u["user_id"], reviewed_at=t, review_note=note, updated_at=t)
        self.store.add_audit(u, "station_proposal_rejected", p["gtfs_id"], None,
                             {"proposal_id": p["proposal_id"], "name": p["name"], "note": note})
        return self.proposal_detail(p)

    def reopen_proposal(self, u, p):
        self.require_role(u, "editor")
        if p["status"] != "rejected":
            raise ApiError(409, "proposal_not_rejected", f"Only a rejected proposal can be reopened; this one is {p['status']}.")
        p.update(status="pending", reviewed_by=None, reviewed_at=None, review_note=None, updated_at=iso(now()))
        self.store.add_audit(u, "station_proposal_reopened", p["gtfs_id"], None,
                             {"proposal_id": p["proposal_id"], "name": p["name"]})
        return self.proposal_detail(p)

    def return_proposals(self, u, cs, reason, change_id=None):
        """A proposal whose change left its draft is waiting for review again."""
        for p in self.store.proposals.values():
            if p["change_set_id"] != cs["change_set_id"] or p["status"] != "approved":
                continue
            if change_id is not None and p["change_id"] != change_id:
                continue
            p.update(status="pending", change_set_id=None, change_id=None, reviewed_by=None, reviewed_at=None,
                     updated_at=iso(now()))
            self.store.add_audit(u, "station_proposal_returned", p["gtfs_id"], cs["change_set_id"],
                                 {"proposal_id": p["proposal_id"], "station_id": p["station_id"], "reason": reason})

    # ---- coordinate reviews (docs sections 8 and 8.1, as the API answers them)
    def review_out(self, rv):
        s = self.store
        cs = s.change_sets.get(rv["change_set_id"]) if rv.get("change_set_id") else None
        return {k: rv.get(k) for k in ("review_id", "gtfs_id", "stop_id", "original_stop_id", "stop_name", "reason",
                                       "lat", "lon", "raw_lat", "raw_lon", "suggested_lat", "suggested_lon",
                                       "suggested_source", "evidence", "status", "change_set_id", "change_id",
                                       "reviewed_at", "review_note", "batch", "created_at", "updated_at")} | {
            "change_set_title": cs["title"] if cs else None, "reviewed_by_email": s.email_of(rv.get("reviewed_by"))}

    def review_problems(self, rv, proj=None):
        """What stands in the way of changing the review's stop: the live data, or
        with `proj` the live data with a draft applied."""
        s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
        live = s.stops.get((g, sid))
        if not live:
            return [{"level": "error", "code": "stop_missing", "message": f"stop {sid} no longer exists"}]
        if live.get("deleted") and live.get("merged_into"):
            into = live["merged_into"]
            return [{"level": "error", "code": "stop_merged_away",
                     "message": f"stop {sid} was merged into {into}; its routes call at {into} now"}]
        if live.get("deleted"):
            return [{"level": "error", "code": "stop_deleted", "message": f"stop {sid} is deleted"}]
        if live.get("location_type") == 1:
            return [{"level": "error", "code": "stop_is_station", "message": f"{sid} is a station, not a stop"}]
        drafted = proj.stop(sid) if proj else live
        if drafted.get("deleted") and drafted.get("merged_into"):
            return [{"level": "error", "code": "stop_merged_away",
                     "message": f"stop {sid} is merged into {drafted['merged_into']} in the draft"}]
        if drafted.get("deleted"):
            return [{"level": "error", "code": "stop_deleted", "message": f"stop {sid} is deleted in the draft"}]
        moved = haversine(rv["lat"], rv["lon"], drafted["lat"], drafted["lon"])
        if rv["status"] != "committed" and moved > REVIEW_MOVED_M:
            return [{"level": "warning", "code": "moved_since_load",
                     "message": f"stop {sid} is {moved:.0f} m from where the review found it"}]
        return []

    def review_actions(self, rv):
        """The review's moves and splits in the draft that carries it, in change order."""
        s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
        cs = s.change_sets.get(rv.get("change_set_id")) if rv.get("change_set_id") else None
        if not cs or rv["status"] not in ("approved", "committed"):
            return []
        mine = [c for c in cs["changes"] if (c.get("after") or {}).get("position_review_id") == rv["review_id"]]
        calls = route_legs(s, g, sid)
        out = []
        for c in mine:
            a = c["after"]
            if c["entity"] == "stop" and c["op"] == "update":
                out.append({"kind": "move", "change_id": c["change_id"], "lat": a["lat"], "lon": a["lon"],
                            "detour_m_after": detour_of(calls, sid, a["lat"], a["lon"])})
            elif c["entity"] == "stop" and c["op"] == "create":
                new_id = c["entity_key"]
                ids = [x["entity_key"] for x in mine if x["entity"] == "route_stops"
                       and any(r.get("stop_id") == new_id for r in (x.get("after") or {}).get("rows", []))]
                if rv["status"] == "committed":   # the split routes call at the new stop now
                    after = detour_of(route_legs(s, g, new_id), new_id, a["lat"], a["lon"], only=ids)
                else:
                    after = detour_of(calls, sid, a["lat"], a["lon"], only=ids)
                out.append({"kind": "split", "change_id": c["change_id"], "lat": a["lat"], "lon": a["lon"],
                            "new_stop_id": new_id, "route_ids": ids, "detour_m_after": after})
            elif c["entity"] == "stop" and c["op"] == "merge":   # round 4 (UX)
                out.append(self.merge_action(rv, c, calls))
        return out

    def review_detail(self, rv, q=None):
        s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
        st = s.stops.get((g, sid))
        calls = route_legs(s, g, sid)
        here = (st["lat"], st["lon"]) if st else None
        stop = None
        if st:
            stop = stop_out(s, g, st)
            if st.get("merged_into"):
                stop["provenance"] = {"merged_into": st["merged_into"]}
        actions = self.review_actions(rv)
        latest = actions[-1] if actions else None
        detour_after = latest["detour_m_after"] if latest else None
        if q and (q.get("lat") or q.get("lon") or q.get("route_ids")):
            # what a point would give, before anything is drafted
            try:
                lat, lon = float(q["lat"][0]), float(q["lon"][0])
            except (KeyError, ValueError):
                raise ApiError(400, "invalid_position", "lat and lon go together, and route_ids needs a point.")
            if not valid_position(lat, lon):
                raise ApiError(400, "invalid_position", "lat and lon must be a valid position")
            only = [x.strip() for x in q["route_ids"][0].split(",") if x.strip()] if q.get("route_ids") else None
            detour_after = detour_of(calls, sid, lat, lon, only=only)
        return self.review_out(rv) | {
            "stop": stop,
            "routes": [dict(c, detour_m=(tenth(leg_detour(c, sid, *here)) if here else None)) for c in calls],
            "detour_m": detour_of(calls, sid, *here) if here else None,
            "new_position": {"lat": latest["lat"], "lon": latest["lon"]} if latest else None,
            "new_stop_id": latest.get("new_stop_id") if latest else None,
            "split_route_ids": latest.get("route_ids") if latest else None,
            "detour_m_after": detour_after,
            "draft_actions": actions if rv["status"] == "approved" else [],
            "problems": self.review_problems(rv)}

    def review_summary(self, g):
        counts = {k: 0 for k in ("pending", "approved", "committed", "confirmed")}
        for rv in self.store.reviews.values():
            if rv["gtfs_id"] == g and rv["status"] in counts:
                counts[rv["status"]] += 1
        counts["auto_fix"] = self.auto_fix_counts(g)   # round 4 (UX)
        return counts

    def list_reviews(self, g, q):
        statuses = [x for x in (q.get("status", [""])[0] or "pending").split(",") if x]
        bad = [x for x in statuses if x not in REVIEW_STATUSES]
        if bad:
            raise ApiError(400, "invalid_status", f"Unknown status {bad[0]}.")
        items = [rv for rv in self.store.reviews.values() if rv["gtfs_id"] == g and rv["status"] in statuses]
        items = self.filter_auto_fix(items, q)   # round 4 (UX)
        if q.get("bbox"):
            try:
                a, b, c, d = (float(x) for x in q["bbox"][0].split(","))
            except ValueError:
                raise ApiError(400, "bad_bbox", "bbox is minLat,minLon,maxLat,maxLon.")
            items = [rv for rv in items if a <= rv["lat"] <= c and b <= rv["lon"] <= d]
        term = (q.get("q", [""])[0] or "").strip().lower()
        order = {k: i for i, k in enumerate(REVIEW_STATUSES)}
        if term:
            def rank(rv):
                if term in (rv["stop_id"].lower(), rv["original_stop_id"].lower()):
                    return 0
                n = rv["stop_name"].lower()
                return 1 if n.startswith(term) else 2 if term in n else 9
            items = sorted((rv for rv in items if rank(rv) < 9), key=lambda rv: (rank(rv), order[rv["status"]], rv["review_id"]))
        else:
            items.sort(key=lambda rv: (order[rv["status"]], rv["review_id"]))
        page = paginate(items, q)
        page["items"] = [self.review_out(rv) for rv in page["items"]]
        return page

    @staticmethod
    def review_note(b):
        note = b.get("note")
        if note is not None and not isinstance(note, str):
            raise ApiError(400, "invalid_json", "note must be text.")
        return (note or "").strip() or None

    def action_draft(self, u, rv, b):
        """The draft a move or split goes into: the review is waiting, or already has
        a change in that same draft (a review may take several)."""
        cs = self.target_draft(u, rv["gtfs_id"], b)
        if rv["status"] == "approved" and rv["change_set_id"] != cs["change_set_id"]:
            other = self.store.change_sets.get(rv["change_set_id"]) or {}
            raise ApiError(409, "review_in_other_draft",
                           f"position review {rv['review_id']} has changes in draft “{other.get('title', '')}”; "
                           f"add more to that draft",
                           {"change_set_id": rv["change_set_id"], "change_set_title": other.get("title")})
        if rv["status"] not in ("pending", "approved"):
            raise ApiError(409, "review_not_pending", f"position review {rv['review_id']} is {rv['status']}",
                           {"status": rv["status"], "change_set_id": rv["change_set_id"]})
        return cs

    def approve_with(self, u, rv, cs, change_id, note):
        t = iso(now())
        rv.update(status="approved", change_set_id=cs["change_set_id"], change_id=change_id,
                  reviewed_by=u["user_id"], reviewed_at=t, review_note=note or rv.get("review_note"), updated_at=t)

    def move_review(self, u, rv, b):
        s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
        lat, lon = b.get("lat"), b.get("lon")
        if not valid_position(lat, lon):
            raise ApiError(400, "invalid_position", "lat and lon must be a valid position")
        note = self.review_note(b)
        cs = self.action_draft(u, rv, b)
        moves = [c["change_id"] for c in cs["changes"] if c["entity"] == "stop" and c["op"] == "update"
                 and (c.get("after") or {}).get("position_review_id") == rv["review_id"]]
        if moves:
            raise ApiError(409, "draft_conflict",
                           f"change {moves[0]} in this draft already moves stop {sid}; edit that change in the draft",
                           {"change_ids": moves})
        found = self.review_problems(rv, Projection(s, g, cs["changes"]))
        if any(p["level"] == "error" for p in found):
            raise ApiError(400, "review_has_problems", "the stop cannot be changed now; see the problems",
                           {"problems": found})
        st = s.stops[(g, sid)]
        moved = haversine(st["lat"], st["lon"], lat, lon)
        if moved < 0.5:
            raise ApiError(400, "position_unchanged",
                           f"stop {sid} is already at that point; if its position is right, confirm the review")
        before, live_version = self.snapshot(cs, "stop", sid)
        ch = self.append_change(u, cs, "stop", "update", sid, {"lat": lat, "lon": lon, "position_review_id": rv["review_id"]},
                                before, live_version)
        self.approve_with(u, rv, cs, ch["change_id"], note)
        calls = route_legs(s, g, sid)
        s.add_audit(u, "position_review_moved", g, cs["change_set_id"], {
            "review_id": rv["review_id"], "stop_id": sid, "change_id": ch["change_id"],
            "from": {"lat": st["lat"], "lon": st["lon"]}, "to": {"lat": lat, "lon": lon}, "moved_m": tenth(moved),
            "detour_m": detour_of(calls, sid, st["lat"], st["lon"]), "detour_m_after": detour_of(calls, sid, lat, lon),
            "note": note})
        return self.review_detail(rv)

    def split_review(self, u, rv, b):
        """Split some routes off the reviewed stop onto a new stop at the given point
        (docs section 8.1): a stop/create plus one route_stops/replace per route."""
        s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
        route_ids = [str(x).strip() for x in b.get("route_ids") or []] if isinstance(b.get("route_ids"), list) else []
        lat, lon = b.get("lat"), b.get("lon")
        found = []
        if not route_ids:
            found.append({"code": "route_ids_required", "message": "name the routes to split off the stop"})
        seen = set()
        for rid in route_ids:
            if rid in seen and not any(p.get("route_id") == rid and p["code"] == "route_listed_twice" for p in found):
                found.append({"code": "route_listed_twice", "route_id": rid, "message": f"route {rid} is listed more than once"})
            seen.add(rid)
        if not valid_position(lat, lon):
            found.append({"code": "invalid_position", "message": f"position {lat}, {lon} is out of range"})
        if found:
            raise ApiError(400, "invalid_split", "the routes cannot be split off this way; see the problems",
                           {"problems": found})
        name, note = b.get("name"), self.review_note(b)
        if name is not None and not isinstance(name, str):
            raise ApiError(400, "invalid_json", "name must be text.")
        cs = self.action_draft(u, rv, b)
        clashing = [c["change_id"] for c in cs["changes"]
                    if (c["entity"] == "route_stops" and c["entity_key"] in route_ids)
                    or (c["entity"] == "stop" and c["entity_key"] == sid)
                    or (c["entity"] == "stop" and c["op"] == "merge" and (c.get("after") or {}).get("into_stop_id") == sid)]
        if clashing:
            raise ApiError(409, "draft_conflict",
                           f"change(s) {', '.join(map(str, clashing))} in this draft already change stop {sid} or "
                           f"these routes' stop lists", {"change_ids": clashing})
        problems = [p for p in self.review_problems(rv) if p["level"] == "error"]
        if problems:
            raise ApiError(400, "review_has_problems", "the stop cannot be changed now; see the problems",
                           {"problems": problems})
        st = s.stops[(g, sid)]
        calls = route_legs(s, g, sid)
        calling = {c["route_id"] for c in calls}
        found = [{"code": "route_not_at_stop", "route_id": rid, "message": f"route {rid} does not call at stop {sid}"}
                 for rid in route_ids if rid not in calling]
        if calling and calling <= set(route_ids):
            found.append({"code": "would_empty_stop",
                          "message": f"every route calling at stop {sid} would leave it; move the stop instead"})
        if found:
            raise ApiError(400, "invalid_split", "the routes cannot be split off this way; see the problems",
                           {"problems": found})
        new_id = s.mint_stop_id(g)
        name = (name or "").strip() or st["name"]
        create = self.append_change(u, cs, "stop", "create", new_id,
                                    {"stop_id": new_id, "name": name, "lat": lat, "lon": lon,
                                     "position_review_id": rv["review_id"]})
        proj = Projection(s, g, [])
        route_change_ids = []
        for rid in route_ids:
            live_rows = s.rows.get((g, rid), [])
            rows = [{"stop_id": new_id if r.get("stop_id") == sid else r.get("stop_id"),
                     "stop_type": r["stop_type"], "stage_no": r["stage_no"], "stage_name": r["stage_name"],
                     "marker_id": r.get("marker_id"), "marker_name": r.get("marker_name"),
                     "marker_lat": r.get("marker_lat"), "marker_lon": r.get("marker_lon"),
                     "stop_name_override": r.get("stop_name_override"), "provider_id": r.get("provider_id")}
                    for r in live_rows]
            ch = self.append_change(u, cs, "route_stops", "replace", rid,
                                    {"base_rows_hash": rows_hash(live_rows), "rows": rows,
                                     "position_review_id": rv["review_id"]},
                                    route_detail(proj, rid)["rows"])
            route_change_ids.append(ch["change_id"])
        self.approve_with(u, rv, cs, create["change_id"], note)
        after = detour_of(calls, sid, lat, lon, only=route_ids)
        s.add_audit(u, "position_review_split", g, cs["change_set_id"], {
            "review_id": rv["review_id"], "stop_id": sid, "new_stop_id": new_id, "route_ids": route_ids,
            "change_set_id": cs["change_set_id"], "change_id": create["change_id"], "route_change_ids": route_change_ids,
            "name": name, "lat": lat, "lon": lon, "detour_m_after": after, "note": note})
        return self.review_detail(rv) | {"new_stop_id": new_id, "detour_m_after": after}

    def confirm_review(self, u, rv, b):
        self.require_role(u, "editor")
        note = self.review_note(b)
        if rv["status"] != "pending":
            raise ApiError(409, "review_not_pending", f"position review {rv['review_id']} is {rv['status']}",
                           {"status": rv["status"], "change_set_id": rv["change_set_id"]})
        t = iso(now())
        st = self.store.stops.get((rv["gtfs_id"], rv["stop_id"]))
        rv.update(status="confirmed", reviewed_by=u["user_id"], reviewed_at=t, review_note=note, updated_at=t)
        self.store.add_audit(u, "position_review_confirmed", rv["gtfs_id"], None,
                             {"review_id": rv["review_id"], "stop_id": rv["stop_id"],
                              "position": {"lat": st["lat"], "lon": st["lon"]} if st else None, "note": note})
        return self.review_detail(rv)

    def reopen_review(self, u, rv):
        self.require_role(u, "editor")
        if rv["status"] != "confirmed":
            raise ApiError(409, "review_not_confirmed",
                           f"position review {rv['review_id']} is {rv['status']}; only a confirmed review is reopened")
        if any(o is not rv and o["gtfs_id"] == rv["gtfs_id"] and o["stop_id"] == rv["stop_id"]
               and o["status"] in ("pending", "approved") for o in self.store.reviews.values()):
            raise ApiError(409, "review_superseded", f"another open review already covers stop {rv['stop_id']}")
        note = rv.get("review_note")
        rv.update(status="pending", reviewed_by=None, reviewed_at=None, review_note=None, updated_at=iso(now()))
        self.store.add_audit(u, "position_review_reopened", rv["gtfs_id"], None,
                             {"review_id": rv["review_id"], "stop_id": rv["stop_id"], "note": note})
        return self.review_detail(rv)

    def review_change_removed(self, u, cs, ch):
        """A change of a review left its draft: a split's new stop takes its stop
        lists along; a review with no move or split left is waiting again."""
        s = self.store
        prid = (ch.get("after") or {}).get("position_review_id")
        rv = s.reviews.get(prid) if is_int(prid) else None
        if not rv or rv["status"] != "approved" or rv["change_set_id"] != cs["change_set_id"]:
            return
        removed = []
        if ch["entity"] == "stop" and ch["op"] == "create":
            for c in [c for c in cs["changes"] if c["entity"] == "route_stops"
                      and (c.get("after") or {}).get("position_review_id") == prid
                      and any(r.get("stop_id") == ch["entity_key"] for r in c["after"].get("rows", []))]:
                cs["changes"].remove(c)
                removed.append(c["change_id"])
                s.add_audit(u, "change_removed", cs["gtfs_id"], cs["change_set_id"],
                            {"change_id": c["change_id"], "review_id": prid, "reason": "position_review_returned"})
        left = [c for c in cs["changes"] if c["entity"] == "stop" and (c.get("after") or {}).get("position_review_id") == prid]
        if left:
            rv["change_id"] = left[-1]["change_id"]
            return
        rv.update(status="pending", change_set_id=None, change_id=None, reviewed_by=None, reviewed_at=None,
                  review_note=None, updated_at=iso(now()))
        detail = {"review_id": prid, "stop_id": rv["stop_id"], "reason": "change_removed"}
        if removed:
            detail["removed_change_ids"] = removed
        s.add_audit(u, "position_review_returned", rv["gtfs_id"], cs["change_set_id"], detail)

    def return_reviews(self, u, cs, reason):
        """Every review with changes in a discarded draft is waiting again."""
        for rv in self.store.reviews.values():
            if rv["change_set_id"] != cs["change_set_id"] or rv["status"] != "approved":
                continue
            rv.update(status="pending", change_set_id=None, change_id=None, reviewed_by=None, reviewed_at=None,
                      review_note=None, updated_at=iso(now()))
            self.store.add_audit(u, "position_review_returned", rv["gtfs_id"], cs["change_set_id"],
                                 {"review_id": rv["review_id"], "stop_id": rv["stop_id"], "reason": reason})


# ====================================================================== round 4 (UX), 2026-09-17
# Everything the dashboard's round 4 needs from the API, kept together: the
# cleanup context of a stop and of a route, merging a reviewed stop into a
# same-named one, the clean-up tool's verdict (`evidence.auto_fix`) with its list
# filter and counts, and the fixtures the smoke test opens. The hooks into the
# code above are one line each and marked "round 4 (UX)".
AUTO_FIX_ACTIONS = ("merge", "move", "choose", "none")
AUTO_FIX_THRESHOLD_M = 150
R4_STATION = "stn_r4_shared"


def _auto_fix_of(rv):
    fix = (rv.get("evidence") or {}).get("auto_fix") or {}
    return fix.get("action") if fix.get("action") in AUTO_FIX_ACTIONS else "none"


def auto_fix_counts(self, g):
    """How many reviews still to do carry each verdict of the clean-up tool."""
    counts = {k: 0 for k in AUTO_FIX_ACTIONS}
    for rv in self.store.reviews.values():
        if rv["gtfs_id"] == g and rv["status"] == "pending":
            counts[_auto_fix_of(rv)] += 1
    return counts


def filter_auto_fix(self, items, q):
    want = (q.get("auto_fix", [""])[0] or "").strip()
    if not want:
        return items
    if want not in AUTO_FIX_ACTIONS:
        raise ApiError(400, "invalid_auto_fix", f"auto_fix is one of {', '.join(AUTO_FIX_ACTIONS)}.")
    return [rv for rv in items if _auto_fix_of(rv) == want]


def _mentions(detail, key, value):
    """Does an audit detail name the entity, however it spells the field?"""
    if not isinstance(detail, dict):
        return False
    return any(detail.get(k) == value for k in (key, "entity_key", "from", "into", "new_stop_id")) or \
        value in (detail.get("route_ids") or [])


def entity_context(self, g, kind, key):
    s = self.store
    if getattr(s, "context_off", False):
        raise ApiError(404, "not_found", "No such feed endpoint.")
    open_sets = [cs for cs in s.change_sets.values() if cs["gtfs_id"] == g and cs["status"] in ("draft", "submitted", "approved")]

    def drafts(touches):
        return [{"change_set_id": cs["change_set_id"], "title": cs["title"], "status": cs["status"],
                 "change_id": ch["change_id"], "entity": ch["entity"], "op": ch["op"]}
                for cs in open_sets for ch in cs["changes"] if touches(ch)]

    def history(field):
        found = [a for a in reversed(s.audit) if a["gtfs_id"] in (g, None) and _mentions(a["detail"], field, key)]
        return [{k: a[k] for k in ("audit_id", "at", "actor_email", "action", "change_set_id", "detail")} for a in found[:10]]

    if kind == "stops":
        st = s.stops.get((g, key))
        if not st:
            raise ApiError(404, "unknown_stop", f"Stop {key} does not exist.")
        calls = route_legs(s, g, key)
        reviews = sorted((rv for rv in s.reviews.values() if rv["gtfs_id"] == g and rv["stop_id"] == key),
                         key=lambda rv: rv["review_id"])
        counts = {k: sum(1 for rv in reviews if rv["status"] == k) for k in ("pending", "approved", "committed", "confirmed")}
        want = norm_name(st["name"])
        same = []
        for (gg, oid), o in s.stops.items():
            if gg != g or oid == key or o.get("deleted") or abs(o["lat"] - st["lat"]) > 0.006:
                continue
            other = norm_name(o["name"])
            alike = 1.0 if other == want else 0.8 if want and other and (want in other or other in want) else 0
            d = haversine(st["lat"], st["lon"], o["lat"], o["lon"])
            if alike and d <= 500:
                same.append({"stop_id": oid, "name": o["name"], "lat": o["lat"], "lon": o["lon"], "distance_m": round(d, 1),
                             "route_count": len({rid for rid, _ in s.stop_routes.get((g, oid), [])}),
                             "parent_station": o.get("parent_station"), "location_type": o.get("location_type"),
                             "similarity": alike})
        same.sort(key=lambda x: (-x["similarity"], x["distance_m"]))

        def touches(ch):
            a = ch.get("after") or {}
            members, _ = member_spec(a) if ch["entity"] == "station" else (None, {})
            return (ch["entity"] in ("stop", "station") and ch["entity_key"] == key) or a.get("into_stop_id") == key \
                or key in (members or [])
        return {"stop_id": key, "detour_m": detour_of(calls, key, st["lat"], st["lon"]),
                "routes_measured": sum(1 for c in calls if c["prev"] and c["next"]),
                "position_reviews": dict(counts, items=[{"review_id": rv["review_id"], "status": rv["status"],
                                                         "reason": rv.get("reason")} for rv in reviews]),
                "same_name": same[:12], "audit": history("stop_id"), "open_drafts": drafts(touches)}

    rt = s.routes.get((g, key))
    if not rt:
        raise ApiError(404, "unknown_route", f"Route {key} does not exist.")
    rows = s.rows.get((g, key), [])
    by_stop = {}
    for rv in sorted(s.reviews.values(), key=lambda rv: rv["review_id"]):
        if rv["gtfs_id"] == g and rv["status"] != "superseded":
            by_stop[rv["stop_id"]] = rv
    flagged = [{"stop_id": r["stop_id"], "sequence": r["sequence"], "review_id": by_stop[r["stop_id"]]["review_id"],
                "status": by_stop[r["stop_id"]]["status"]} for r in rows if r.get("stop_id") in by_stop]
    served = [r for r in rows if r["stop_type"] not in SERVED_EXCLUDE and r.get("stop_id") and (g, r["stop_id"]) in s.stops]
    worst = []
    for before, at, after in zip(served, served[1:], served[2:]):
        a, b, c = (s.stops[(g, r["stop_id"])] for r in (before, at, after))
        d = max(0.0, haversine(a["lat"], a["lon"], b["lat"], b["lon"]) + haversine(b["lat"], b["lon"], c["lat"], c["lon"])
                - haversine(a["lat"], a["lon"], c["lat"], c["lon"]))
        if d >= 50:
            worst.append({"stop_id": at["stop_id"], "name": at.get("stop_name_override") or b["name"],
                          "sequence": at["sequence"], "detour_m": tenth(d)})
    worst.sort(key=lambda x: -x["detour_m"])
    return {"route_id": key, "stops_with_reviews": flagged, "worst_detours": worst[:5], "audit": history("route_id"),
            "open_drafts": drafts(lambda ch: ch["entity"] in ("route", "route_stops") and ch["entity_key"] == key)}


def merge_action(self, rv, c, calls):
    """A review's merge as the detail lists it: where the stop that stays is, and
    the detour the reviewed stop's routes would have there."""
    into = self.store.stops.get((rv["gtfs_id"], c["after"]["into_stop_id"])) or {}
    lat, lon = into.get("lat"), into.get("lon")
    return {"kind": "merge", "change_id": c["change_id"], "into_stop_id": c["after"]["into_stop_id"], "lat": lat, "lon": lon,
            "detour_m_after": detour_of(calls, rv["stop_id"], lat, lon) if lat is not None else None}


def merge_review(self, u, rv, b):
    """POST /position-reviews/{id}/merge: the reviewed stop is a duplicate of a
    same-named stop; a stop/merge into it goes into the draft."""
    s, g, sid = self.store, rv["gtfs_id"], rv["stop_id"]
    unknown = set(b) - {"change_set_id", "into_stop_id", "keep_name", "note"}
    if unknown:
        raise ApiError(400, "invalid_json", f"unknown field {sorted(unknown)[0]}")
    into_id = str(b.get("into_stop_id") or "").strip()
    keep_name = b.get("keep_name", "into")
    if keep_name not in ("into", "from"):
        raise ApiError(400, "invalid_json", "keep_name is into or from.")
    note = self.review_note(b)
    cs = self.action_draft(u, rv, b)
    clashing = [c["change_id"] for c in cs["changes"]
                if (c["entity"] == "stop" and c["entity_key"] in (sid, into_id))
                or (c["entity"] == "stop" and c["op"] == "merge" and (c.get("after") or {}).get("into_stop_id") == sid)
                or (c.get("after") or {}).get("position_review_id") == rv["review_id"]]
    if clashing:
        raise ApiError(409, "draft_conflict",
                       f"change(s) {', '.join(map(str, clashing))} in this draft already change stop {sid} or {into_id}",
                       {"change_ids": clashing})
    found = [p for p in self.review_problems(rv, Projection(s, g, cs["changes"])) if p["level"] == "error"]
    into = s.stops.get((g, into_id))
    if not into_id or into_id == sid:
        found.append({"level": "error", "code": "merge_same_stop", "message": "name another stop to merge into"})
    elif not into or into.get("deleted"):
        found.append({"level": "error", "code": "stop_missing", "message": f"stop {into_id} does not exist"})
    elif into.get("location_type") == 1:
        found.append({"level": "error", "code": "stop_is_station", "message": f"{into_id} is a station, not a stop"})
    if found:
        raise ApiError(400, "review_has_problems", "the stop cannot be merged now; see the problems", {"problems": found})
    after = {"into_stop_id": into_id, "into_row_version": into.get("row_version"), "keep_name": keep_name,
             "keep_position": "into", "position_review_id": rv["review_id"]}
    before, live_version = self.snapshot(cs, "stop", sid, "merge", after)
    ch = self.append_change(u, cs, "stop", "merge", sid, after, before, live_version)
    self.approve_with(u, rv, cs, ch["change_id"], note)
    calls = route_legs(s, g, sid)
    st = s.stops[(g, sid)]
    detour_after = detour_of(calls, sid, into["lat"], into["lon"])
    s.add_audit(u, "position_review_merged", g, cs["change_set_id"], {
        "review_id": rv["review_id"], "stop_id": sid, "change_id": ch["change_id"], "into": into_id,
        "keep_name": keep_name, "detour_m": detour_of(calls, sid, st["lat"], st["lon"]),
        "detour_m_after": detour_after, "note": note})
    warnings = [{"level": v["level"], "code": v["code"], "message": v["message"]}
                for v in validate_set(s, cs) if v["change_id"] == ch["change_id"]]
    return self.review_detail(rv) | {"warnings": warnings, "detour_m_after": detour_after}


for _fn in (auto_fix_counts, filter_auto_fix, entity_context, merge_action, merge_review):
    setattr(Handler, _fn.__name__, _fn)


def seed_round4(store):
    """Fixtures for the round 4 flows, taken from the END of the review list so the
    reviews the earlier flows pick (from the start) are left as they were:

      - the last review whose stop shares its point with two stops: those two become
        the platforms of station stn_r4_shared (a station listed once, not twice),
        it gets same-named candidates, and the tool's verdict is "merge";
      - the two reviews before it get the verdicts "move" and "choose".
    """
    g = next(iter(store.feeds), None)
    pending = sorted((rv for rv in store.reviews.values() if rv["gtfs_id"] == g and rv["status"] == "pending"
                      and rv["stop_id"] != Store.FIXTURE_STOP), key=lambda rv: -rv["review_id"])

    def usable(rv):
        st = store.stops.get((g, rv["stop_id"]))
        return st and not st.get("deleted") and st.get("location_type") == 0 and not st.get("parent_station") \
            and any(c["prev"] and c["next"] for c in route_legs(store, g, rv["stop_id"]))

    def shares_of(rv):
        out = []
        for x in (rv.get("evidence") or {}).get("shares_point_with") or []:
            o = store.stops.get((g, x.get("stop_id")))
            if o and not o.get("deleted") and o.get("location_type") == 0 and not o.get("parent_station") \
                    and x["stop_id"] != rv["stop_id"]:
                out.append(o)
        return out

    pending = [rv for rv in pending if usable(rv)]
    main = next((rv for rv in pending if len(shares_of(rv)) >= 2), None)
    if not main:
        return
    st = store.stops[(g, main["stop_id"])]
    platforms = shares_of(main)[:2]
    store.stops[(g, R4_STATION)] = {
        "gtfs_id": g, "stop_id": R4_STATION, "stop_code": R4_STATION, "name": f"{platforms[0]['name']} (station)",
        "lat": platforms[0]["lat"], "lon": platforms[0]["lon"], "location_type": 1, "parent_station": None,
        "platform_code": None, "cluster_id": None, "regional_name": None, "hindi_name": None,
        "position_source": "mock-fixture", "row_version": 1, "deleted": False}
    for n, o in enumerate(platforms, start=1):
        o["parent_station"], o["platform_code"] = R4_STATION, f"Platform {n}"

    calls = route_legs(store, g, st["stop_id"])
    leg = next(c for c in calls if c["prev"] and c["next"])
    now = detour_of(calls, st["stop_id"], st["lat"], st["lon"])
    want = norm_name(st["name"])
    taken = {st["stop_id"], *(o["stop_id"] for o in shares_of(main)), *(o["stop_id"] for o in platforms)}
    named = sorted(((haversine(st["lat"], st["lon"], o["lat"], o["lon"]), o) for (gg, oid), o in store.stops.items()
                    if gg == g and oid not in taken and not o.get("deleted") and o.get("location_type") == 0
                    and norm_name(o["name"]) == want), key=lambda x: (x[0], x[1]["stop_id"]))
    pool = [o for _, o in named[:2]]
    # no same-named stop elsewhere: the stops either side on one of its routes stand in
    for n in (leg["prev"], leg["next"]):
        o = store.stops.get((g, n["stop_id"]))
        if len(pool) < 3 and o and o["stop_id"] not in taken and o not in pool and not o.get("deleted"):
            pool.append(o)
    on_routes = {c["route_id"] for c in calls}
    candidates = []
    for o in pool:
        after = detour_of(calls, st["stop_id"], o["lat"], o["lon"])
        candidates.append({
            "stop_id": o["stop_id"], "name": o["name"], "lat": o["lat"], "lon": o["lon"],
            "distance_m": round(haversine(st["lat"], st["lon"], o["lat"], o["lon"]), 1),
            "name_similarity": 1.0 if norm_name(o["name"]) == want else 0.6,
            "route_count": len({rid for rid, _ in store.stop_routes.get((g, o["stop_id"]), [])}),
            "detour_m_after": after,
            "shares_route": bool(on_routes & {rid for rid, _ in store.stop_routes.get((g, o["stop_id"]), [])}),
            "verdict": "fits" if after is not None and after <= AUTO_FIX_THRESHOLD_M else "no_fit"})
    candidates.sort(key=lambda c: (c["verdict"] != "fits", c["detour_m_after"] if c["detour_m_after"] is not None else 1e12))
    best = candidates[0] if candidates else None
    main["evidence"] = dict(main.get("evidence") or {}, same_name_candidates=candidates, auto_fix={
        "action": "merge" if best else "none", "into_stop_id": best["stop_id"] if best else None,
        "detour_m": now, "detour_m_after": best["detour_m_after"] if best else None,
        "reason": (f"{best['name']} ({best['stop_id']}) is where its routes pass: merging there takes the detour away."
                   if best else "No same-named stop fits its routes."),
        "tool": "mock coordinate_autofix", "threshold_m": AUTO_FIX_THRESHOLD_M})

    others = [rv for rv in pending if rv is not main][:2]
    for rv, action in zip(others, ("move", "choose")):
        o = store.stops[(g, rv["stop_id"])]
        legs = route_legs(store, g, o["stop_id"])
        lg = next(c for c in legs if c["prev"] and c["next"])
        mid = ((lg["prev"]["lat"] + lg["next"]["lat"]) / 2, (lg["prev"]["lon"] + lg["next"]["lon"]) / 2)
        fix = {"action": action, "detour_m": detour_of(legs, o["stop_id"], o["lat"], o["lon"]),
               "tool": "mock coordinate_autofix", "threshold_m": AUTO_FIX_THRESHOLD_M}
        if action == "move":
            fix.update(lat=round(mid[0], 7), lon=round(mid[1], 7), detour_m_after=detour_of(legs, o["stop_id"], *mid),
                       reason=f"Halfway between {lg['prev']['name']} and {lg['next']['name']} its routes barely detour.")
        else:
            fix.update(reason="Two places would fit its routes equally well; a person has to choose.")
        rv["evidence"] = dict(rv.get("evidence") or {}, auto_fix=fix)
# ====================================================================== end of round 4 (UX)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--sample", default=str(HERE / "sample.json.gz"))
    args = ap.parse_args()
    if not Path(args.sample).exists():
        raise SystemExit(f"{args.sample} is missing. Run dev/export_sample.py against the local Postgres first.")
    with gzip.open(args.sample, "rt", encoding="utf-8") as fh:
        Handler.store = Store(json.load(fh))
    seed_round4(Handler.store)
    print(f"GTFS editor mock on http://127.0.0.1:{args.port}{UI}/")
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
