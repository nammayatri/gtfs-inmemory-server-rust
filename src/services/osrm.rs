//! The OSRM calls the GTFS editor makes (docs/gtfs-editor.md sections 3 and 17):
//!
//! - [`route_through`] - a road route through a route's stops, in chunks of at
//!   most [`ROUTE_CHUNK`] waypoints that overlap by one, so a long route is not
//!   one fragile request; and when it fails it says why - which leg OSRM found
//!   no road for, which waypoint it could not place on a road, or whether it
//!   simply did not answer.
//! - [`match_path`] - OSRM `/match` over a path traced from GPS: resampled
//!   every ~50 m, in chunks of at most [`MATCH_CHUNK`] points that overlap, and
//!   stitched back together. A stretch OSRM cannot match keeps the GPS geometry
//!   rather than losing the line, and the answer says how much was matched.
//!
//! Plus the small geometry both need: a local planar projection, and Google's
//! encoded polyline (precision 5), which is what `gtfs_route.encoded_polyline`
//! holds.

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

/// Waypoints per `/route` request. OSRM's own limit is far higher; this keeps
/// one bad stop from costing more than one short request to find.
pub const ROUTE_CHUNK: usize = 25;
/// Points per `/match` request: OSRM's default `max-matching-size`.
pub const MATCH_CHUNK: usize = 100;
/// The longest a single OSRM request may take.
const PER_REQUEST: Duration = Duration::from_secs(12);

// ---------------------------------------------------------------- geometry

/// Equirectangular projection around a fixed origin: metres east and north.
/// Good to well under a metre across a city, which is all it is used for.
#[derive(Debug, Clone, Copy)]
pub struct Planar {
    lat0: f64,
    lon0: f64,
    kx: f64,
    ky: f64,
}

impl Planar {
    pub fn new(lat0: f64, lon0: f64) -> Self {
        Self {
            lat0,
            lon0,
            kx: 111_320.0 * lat0.to_radians().cos(),
            ky: 110_574.0,
        }
    }

    /// Around a point, with the origin rounded to 0.1 degree so that the same
    /// input always projects to the same numbers.
    pub fn around(lat: f64, lon: f64) -> Self {
        Self::new((lat * 10.0).round() / 10.0, (lon * 10.0).round() / 10.0)
    }

    pub fn xy(&self, (lat, lon): (f64, f64)) -> (f64, f64) {
        ((lon - self.lon0) * self.kx, (lat - self.lat0) * self.ky)
    }

    pub fn ll(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (y / self.ky + self.lat0, x / self.kx + self.lon0)
    }
}

pub fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Distance from `p` to segment `a`-`b`, and where along it (0..=1) the
/// nearest point is.
pub fn seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 <= 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    };
    (dist(p, (a.0 + t * dx, a.1 + t * dy)), t)
}

/// Length in metres of a line of (lat, lon).
pub fn length_m(points: &[(f64, f64)]) -> f64 {
    points
        .windows(2)
        .map(|w| crate::editor::validation::haversine_m(w[0].0, w[0].1, w[1].0, w[1].1))
        .sum()
}

/// Google encoded polyline, precision 5.
pub fn encode_polyline(points: &[(f64, f64)]) -> String {
    let mut out = String::new();
    let (mut plat, mut plon) = (0i64, 0i64);
    for &(lat, lon) in points {
        let (ilat, ilon) = ((lat * 1e5).round() as i64, (lon * 1e5).round() as i64);
        for delta in [ilat - plat, ilon - plon] {
            let mut v = if delta < 0 { !(delta << 1) } else { delta << 1 };
            while v >= 0x20 {
                out.push(char::from((0x20 | (v & 0x1f)) as u8 + 63));
                v >>= 5;
            }
            out.push(char::from(v as u8 + 63));
        }
        plat = ilat;
        plon = ilon;
    }
    out
}

pub fn decode_polyline(encoded: &str) -> Option<Vec<(f64, f64)>> {
    crate::editor::validation::decode_polyline(encoded)
}

/// Points every `step_m` along a line of (lat, lon), first and last kept.
pub fn resample(points: &[(f64, f64)], step_m: f64) -> Vec<(f64, f64)> {
    let Some(&first) = points.first() else {
        return vec![];
    };
    let pl = Planar::around(first.0, first.1);
    let xy: Vec<(f64, f64)> = points.iter().map(|&p| pl.xy(p)).collect();
    let mut out = vec![xy[0]];
    let mut carry = 0.0;
    for w in xy.windows(2) {
        let (a, b) = (w[0], w[1]);
        let seg = dist(a, b);
        if seg <= 0.0 {
            continue;
        }
        let mut t = step_m - carry;
        while t <= seg {
            let f = t / seg;
            out.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
            t += step_m;
        }
        carry = seg - (t - step_m);
    }
    let last = *xy.last().expect("non-empty");
    if dist(*out.last().expect("non-empty"), last) > 1.0 {
        out.push(last);
    } else if out.len() > 1 {
        *out.last_mut().expect("non-empty") = last;
    }
    out.into_iter().map(|p| pl.ll(p)).collect()
}

// ---------------------------------------------------------------- /route

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailReason {
    Timeout,
    NoSegment,
    NoRoute,
    HttpError,
    Unreachable,
}

#[derive(Debug, Clone)]
pub struct RouteFailure {
    pub reason: FailReason,
    /// OSRM's own `code` (`NoSegment`, `NoRoute`, `InvalidQuery`...), or the
    /// HTTP status when it did not send one.
    pub osrm_code: Option<String>,
    pub message: String,
    /// The leg with no road: from waypoint `leg` to waypoint `leg + 1`.
    pub leg: Option<usize>,
    /// The waypoint OSRM could not place on any road (`NoSegment`).
    pub waypoint: Option<usize>,
}

impl RouteFailure {
    fn new(reason: FailReason, osrm_code: Option<String>, message: impl Into<String>) -> Self {
        Self {
            reason,
            osrm_code,
            message: message.into(),
            leg: None,
            waypoint: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutedLine {
    pub points: Vec<(f64, f64)>,
    /// (distance m, duration s) per leg, one per consecutive waypoint pair.
    pub legs: Vec<(f64, f64)>,
}

static COORDINATE_N: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)coordinate\s+(\d+)").expect("valid regex"));

fn coords_path(coords: &[(f64, f64)]) -> String {
    coords
        .iter()
        .map(|(lat, lon)| format!("{:.6},{:.6}", lon, lat))
        .collect::<Vec<_>>()
        .join(";")
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
}

/// GET an OSRM URL: its JSON (whatever the status), or a failure.
async fn get_json(
    http: &reqwest::Client,
    url: &str,
    deadline: Instant,
) -> Result<(u16, Option<Value>), RouteFailure> {
    let Some(left) = remaining(deadline) else {
        return Err(RouteFailure::new(
            FailReason::Timeout,
            None,
            "OSRM ran out of time",
        ));
    };
    let wait = left.min(PER_REQUEST);
    let sent = http.get(url).timeout(wait).send().await;
    let transport = |e: reqwest::Error| {
        if e.is_timeout() {
            RouteFailure::new(
                FailReason::Timeout,
                None,
                format!("OSRM did not answer within {} s", wait.as_secs().max(1)),
            )
        } else {
            RouteFailure::new(FailReason::Unreachable, None, "OSRM could not be reached")
        }
    };
    let resp = sent.map_err(transport)?;
    let status = resp.status().as_u16();
    let text = resp.text().await.map_err(transport)?;
    Ok((status, serde_json::from_str::<Value>(&text).ok()))
}

/// One `/route` request through `coords`, all local indexes.
async fn route_once(
    http: &reqwest::Client,
    base: &str,
    coords: &[(f64, f64)],
    deadline: Instant,
) -> Result<RoutedLine, RouteFailure> {
    let url = format!(
        "{}/route/v1/driving/{}?overview=full&geometries=polyline",
        base.trim_end_matches('/'),
        coords_path(coords)
    );
    let (status, json) = get_json(http, &url, deadline).await?;
    let code = json
        .as_ref()
        .and_then(|j| j["code"].as_str())
        .map(str::to_string);
    let message = json
        .as_ref()
        .and_then(|j| j["message"].as_str())
        .unwrap_or("")
        .to_string();
    match (status, code.as_deref(), json.as_ref()) {
        (200, Some("Ok"), Some(j)) => {
            let route = &j["routes"][0];
            let points = route["geometry"]
                .as_str()
                .and_then(decode_polyline)
                .filter(|p| p.len() >= 2)
                .ok_or_else(|| {
                    RouteFailure::new(
                        FailReason::HttpError,
                        code.clone(),
                        "OSRM answered without a route geometry",
                    )
                })?;
            let legs = route["legs"]
                .as_array()
                .map(|legs| {
                    legs.iter()
                        .map(|l| {
                            (
                                l["distance"].as_f64().unwrap_or(0.0),
                                l["duration"].as_f64().unwrap_or(0.0),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(RoutedLine { points, legs })
        }
        (_, Some("NoSegment"), _) => {
            let mut f = RouteFailure::new(
                FailReason::NoSegment,
                code.clone(),
                if message.is_empty() {
                    "OSRM found no road near a waypoint".to_string()
                } else {
                    message.clone()
                },
            );
            f.waypoint = COORDINATE_N
                .captures(&message)
                .and_then(|c| c[1].parse::<usize>().ok());
            Err(f)
        }
        (_, Some("NoRoute"), _) => Err(RouteFailure::new(
            FailReason::NoRoute,
            code.clone(),
            if message.is_empty() {
                "OSRM found no road route".to_string()
            } else {
                message.clone()
            },
        )),
        (_, Some(c), _) => Err(RouteFailure::new(
            FailReason::HttpError,
            Some(c.to_string()),
            format!("OSRM answered HTTP {status}: {c}: {message}"),
        )),
        (_, None, _) => Err(RouteFailure::new(
            FailReason::HttpError,
            Some(status.to_string()),
            format!("OSRM answered HTTP {status} without a JSON body"),
        )),
    }
}

/// A road route through `coords` (lat, lon), in order, in chunks of at most
/// [`ROUTE_CHUNK`] waypoints overlapping by one. A failure names the leg or
/// the waypoint (global indexes) where it can.
pub async fn route_through(
    http: &reqwest::Client,
    base: &str,
    coords: &[(f64, f64)],
    budget: Duration,
) -> Result<RoutedLine, RouteFailure> {
    let n = coords.len();
    if n < 2 {
        return Err(RouteFailure::new(
            FailReason::NoRoute,
            None,
            "a route needs at least two stops with a position",
        ));
    }
    let deadline = Instant::now() + budget;
    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut legs = Vec::new();
    let mut start = 0;
    while start < n - 1 {
        let end = (start + ROUTE_CHUNK - 1).min(n - 1);
        match route_once(http, base, &coords[start..=end], deadline).await {
            Ok(line) => {
                // the chunks share a waypoint; its point is already there
                let skip = usize::from(!points.is_empty());
                points.extend(line.points.into_iter().skip(skip));
                legs.extend(line.legs);
            }
            Err(mut f) => {
                f.waypoint = f.waypoint.map(|w| start + w);
                if f.reason == FailReason::NoRoute {
                    // find the leg: ask for each one on its own
                    for i in start..end {
                        match route_once(http, base, &coords[i..=i + 1], deadline).await {
                            Ok(_) => continue,
                            Err(g) if g.reason == FailReason::NoRoute => {
                                f.leg = Some(i);
                                f.message = g.message;
                                break;
                            }
                            Err(mut g) if g.reason == FailReason::NoSegment => {
                                g.waypoint = g.waypoint.map(|w| i + w);
                                f = g;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                }
                return Err(f);
            }
        }
        start = end;
    }
    Ok(RoutedLine { points, legs })
}

// ---------------------------------------------------------------- /match

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchQuality {
    /// Every stretch was snapped to roads by OSRM.
    Osrm,
    /// Some stretches kept the GPS geometry.
    Partial,
    /// OSRM matched nothing, or is not configured: the GPS path as it is.
    None,
}

#[derive(Debug, Clone)]
pub struct MatchOptions {
    /// Spacing of the points sent to OSRM.
    pub step_m: f64,
    /// Points per request.
    pub chunk: usize,
    /// Points shared by two neighbouring requests.
    pub overlap: usize,
    /// How far OSRM may move a point (`radiuses`).
    pub radius_m: f64,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            step_m: 50.0,
            chunk: MATCH_CHUNK,
            overlap: 8,
            radius_m: 25.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Matched {
    pub points: Vec<(f64, f64)>,
    pub quality: MatchQuality,
    /// Share of the line's length that OSRM matched, 0..=1.
    pub matched_share: f64,
    pub chunks: usize,
    pub chunks_failed: usize,
    /// Stretches OSRM matched with a detour the buses did not take, where the
    /// GPS geometry was kept.
    pub detours: usize,
    /// The first thing that went wrong, when something did.
    pub error: Option<String>,
}

/// Where the chunks start and end (inclusive point indexes), and each
/// chunk's own stretch: consecutive chunks meet in the middle of their
/// overlap, where both have context on either side.
pub fn chunk_plan(n: usize, chunk: usize, overlap: usize) -> Vec<((usize, usize), (usize, usize))> {
    if n < 2 {
        return vec![];
    }
    let chunk = chunk.max(overlap + 2).max(2);
    let mut spans = Vec::new();
    let mut start = 0;
    loop {
        let end = (start + chunk - 1).min(n - 1);
        spans.push((start, end));
        if end == n - 1 {
            break;
        }
        start = end + 1 - overlap.max(1);
    }
    let mut cuts = vec![0];
    for w in spans.windows(2) {
        let (next_start, end) = (w[1].0, w[0].1);
        cuts.push(next_start + (end - next_start) / 2);
    }
    cuts.push(n - 1);
    spans
        .iter()
        .enumerate()
        .map(|(i, &span)| (span, (cuts[i], cuts[i + 1])))
        .collect()
}

/// One chunk's answer: per input point, the matching it went into and where
/// OSRM put it; and each matching's geometry.
#[derive(Debug, Default, Clone)]
pub struct ChunkMatch {
    pub tracepoints: Vec<Option<(usize, (f64, f64))>>,
    pub matchings: Vec<Vec<(f64, f64)>>,
}

pub fn parse_match(json: &Value, n: usize) -> Option<ChunkMatch> {
    if json["code"].as_str() != Some("Ok") {
        return None;
    }
    let matchings: Vec<Vec<(f64, f64)>> = json["matchings"]
        .as_array()?
        .iter()
        .map(|m| {
            m["geometry"]
                .as_str()
                .and_then(decode_polyline)
                .unwrap_or_default()
        })
        .collect();
    let tps = json["tracepoints"].as_array()?;
    let tracepoints = (0..n)
        .map(|i| {
            let t = tps.get(i)?;
            let m = t["matchings_index"].as_u64()? as usize;
            let loc = t["location"].as_array()?;
            let (lon, lat) = (loc.first()?.as_f64()?, loc.get(1)?.as_f64()?);
            (matchings.get(m).is_some_and(|g| g.len() >= 2)).then_some((m, (lat, lon)))
        })
        .collect();
    Some(ChunkMatch {
        tracepoints,
        matchings,
    })
}

/// Where `p` sits on `line`, searching forward from `from` (segment, t):
/// the first segment within 2 m, else the nearest ahead.
fn locate(line: &[(f64, f64)], p: (f64, f64), from: (usize, f64)) -> (usize, f64) {
    let mut best = (f64::INFINITY, from);
    for i in from.0..line.len().saturating_sub(1) {
        let (d, t) = seg_dist(p, line[i], line[i + 1]);
        let t = if i == from.0 { t.max(from.1) } else { t };
        if d < best.0 {
            best = (d, (i, t));
        }
        if d < 2.0 {
            break;
        }
    }
    best.1
}

fn at(line: &[(f64, f64)], (i, t): (usize, f64)) -> (f64, f64) {
    let (a, b) = (line[i], line[(i + 1).min(line.len() - 1)]);
    (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
}

/// A matched stretch longer than this many times the GPS stretch it stands
/// for (plus [`DETOUR_SLACK_M`]) is a detour OSRM invented - typically a
/// point put on the other carriageway, and a loop through the next U-turn to
/// reach it - and the GPS stretch is kept instead.
const DETOUR_FACTOR: f64 = 1.5;
const DETOUR_SLACK_M: f64 = 60.0;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stitched {
    pub line: Vec<(f64, f64)>,
    pub matched_m: f64,
    pub unmatched_m: f64,
    /// Matched stretches refused as detours.
    pub detours: usize,
}

/// Stitch the chunks' answers into one line of planar points. `path` is the
/// resampled GPS path (planar), `plan` from [`chunk_plan`], `answers` one per
/// chunk (None when that request failed).
pub fn stitch(
    path: &[(f64, f64)],
    plan: &[((usize, usize), (usize, usize))],
    answers: &[Option<ChunkMatch>],
) -> Stitched {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let (mut matched, mut unmatched, mut detours) = (0.0, 0.0, 0);
    let mut push = |out: &mut Vec<(f64, f64)>, p: (f64, f64), is_match: bool| {
        if let Some(&last) = out.last() {
            let d = dist(last, p);
            if d < 0.05 {
                return;
            }
            if is_match {
                matched += d;
            } else {
                unmatched += d;
            }
        }
        out.push(p);
    };
    for (((start, _), (a, b)), answer) in plan.iter().zip(answers) {
        let Some(answer) = answer else {
            for p in &path[*a..=*b] {
                push(&mut out, *p, false);
            }
            continue;
        };
        // (input index, matching, position on it)
        let mut prev: Option<(usize, usize, (usize, f64))> = None;
        for i in *a..=*b {
            let Some(Some((m, loc))) = answer.tracepoints.get(i - start) else {
                continue;
            };
            let geom = &answer.matchings[*m];
            match prev {
                Some((pi, pm, ppos)) if pm == *m => {
                    let pos = locate(geom, *loc, ppos);
                    let mut piece: Vec<(f64, f64)> = geom[ppos.0 + 1..=pos.0].to_vec();
                    piece.push(at(geom, pos));
                    let mut piece_m = 0.0;
                    let mut last = at(geom, ppos);
                    for &q in &piece {
                        piece_m += dist(last, q);
                        last = q;
                    }
                    let gps_m: f64 = path[pi..=i].windows(2).map(|w| dist(w[0], w[1])).sum();
                    if piece_m > DETOUR_FACTOR * gps_m + DETOUR_SLACK_M {
                        detours += 1;
                        for q in &path[pi + 1..=i] {
                            push(&mut out, *q, false);
                        }
                    } else {
                        for q in piece {
                            push(&mut out, q, true);
                        }
                    }
                    prev = Some((i, *m, pos));
                }
                _ => {
                    // no anchor yet in this matching: GPS up to here
                    let from = prev.map(|(pi, _, _)| pi + 1).unwrap_or(*a);
                    for p in &path[from..i] {
                        push(&mut out, *p, false);
                    }
                    let pos = locate(geom, *loc, (0, 0.0));
                    push(&mut out, at(geom, pos), false);
                    prev = Some((i, *m, pos));
                }
            }
        }
        let tail = prev.map(|(pi, _, _)| pi + 1).unwrap_or(*a);
        for p in &path[tail..=*b] {
            push(&mut out, *p, false);
        }
    }
    Stitched {
        line: out,
        matched_m: matched,
        unmatched_m: unmatched,
        detours,
    }
}

/// Map-match a path of (lat, lon) traced from GPS. Never fails: without OSRM,
/// or where OSRM cannot match, the GPS geometry stays.
pub async fn match_path(
    http: &reqwest::Client,
    base: Option<&str>,
    path: &[(f64, f64)],
    opts: &MatchOptions,
    budget: Duration,
) -> Matched {
    let as_is = |error: Option<String>| Matched {
        points: path.to_vec(),
        quality: MatchQuality::None,
        matched_share: 0.0,
        chunks: 0,
        chunks_failed: 0,
        detours: 0,
        error,
    };
    let Some(base) = base.map(str::trim).filter(|b| !b.is_empty()) else {
        return as_is(None);
    };
    let Some(&first) = path.first() else {
        return as_is(None);
    };
    let resampled = resample(path, opts.step_m);
    if resampled.len() < 2 {
        return as_is(None);
    }
    let pl = Planar::around(first.0, first.1);
    let plan = chunk_plan(resampled.len(), opts.chunk, opts.overlap);
    let deadline = Instant::now() + budget;
    let mut answers = Vec::with_capacity(plan.len());
    let mut error: Option<String> = None;
    let mut failed = 0;
    for (ci, ((s, e), _)) in plan.iter().enumerate() {
        let pts = &resampled[*s..=*e];
        let url = format!(
            "{}/match/v1/driving/{}?radiuses={}&overview=full&geometries=polyline&tidy=true&gaps=ignore",
            base.trim_end_matches('/'),
            coords_path(pts),
            vec![format!("{:.0}", opts.radius_m); pts.len()].join(";")
        );
        let answer = match get_json(http, &url, deadline).await {
            Ok((_, Some(json))) => match parse_match(&json, pts.len()) {
                Some(m) => Ok(m),
                None => Err(format!(
                    "{}: {}",
                    json["code"].as_str().unwrap_or("error"),
                    json["message"]
                        .as_str()
                        .unwrap_or("OSRM could not match this stretch")
                )),
            },
            Ok((status, None)) => Err(format!("OSRM answered HTTP {status} without a JSON body")),
            Err(f) => Err(f.message),
        };
        match answer {
            Ok(m) => answers.push(Some(ChunkMatch {
                tracepoints: m
                    .tracepoints
                    .into_iter()
                    .map(|t| t.map(|(mi, loc)| (mi, pl.xy(loc))))
                    .collect(),
                matchings: m
                    .matchings
                    .into_iter()
                    .map(|g| g.into_iter().map(|p| pl.xy(p)).collect())
                    .collect(),
            })),
            Err(why) => {
                failed += 1;
                if error.is_none() {
                    error = Some(format!("stretch {} of {}: {why}", ci + 1, plan.len()));
                }
                answers.push(None);
            }
        }
    }
    let path_xy: Vec<(f64, f64)> = resampled.iter().map(|&p| pl.xy(p)).collect();
    let Stitched {
        line,
        matched_m: matched,
        unmatched_m: unmatched,
        detours,
    } = stitch(&path_xy, &plan, &answers);
    let total = matched + unmatched;
    let share = if total > 0.0 { matched / total } else { 0.0 };
    let quality = if failed == 0 && share >= 0.99 {
        MatchQuality::Osrm
    } else if matched > 0.0 {
        MatchQuality::Partial
    } else {
        MatchQuality::None
    };
    if quality == MatchQuality::None {
        return Matched {
            chunks: plan.len(),
            chunks_failed: failed,
            detours,
            ..as_is(error)
        };
    }
    Matched {
        points: line.into_iter().map(|p| pl.ll(p)).collect(),
        quality,
        matched_share: share,
        chunks: plan.len(),
        chunks_failed: failed,
        detours,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_googles_example() {
        let pts = [(38.5, -120.2), (40.7, -120.95), (43.252, -126.453)];
        assert_eq!(encode_polyline(&pts), "_p~iF~ps|U_ulLnnqC_mqNvxq`@");
        assert_eq!(decode_polyline("_p~iF~ps|U_ulLnnqC_mqNvxq`@").unwrap(), pts);
    }

    #[test]
    fn resampling_keeps_the_ends_and_the_spacing() {
        let line = [(13.0, 80.2), (13.0, 80.21), (13.01, 80.21)];
        let r = resample(&line, 50.0);
        let (first, last) = (r[0], *r.last().unwrap());
        assert!((first.0 - 13.0).abs() < 1e-9 && (first.1 - 80.2).abs() < 1e-9);
        assert!((last.0 - 13.01).abs() < 1e-9 && (last.1 - 80.21).abs() < 1e-9);
        let pl = Planar::around(13.0, 80.2);
        for w in r.windows(2) {
            let d = dist(pl.xy(w[0]), pl.xy(w[1]));
            assert!(d <= 51.5, "{d}");
        }
        // ~1.08 km + 1.1 km at 50 m
        assert!((40..=48).contains(&r.len()), "{}", r.len());
    }

    #[test]
    fn chunks_overlap_and_their_own_stretches_tile_the_path() {
        for (n, chunk, overlap) in [
            (2, 100, 8),
            (100, 100, 8),
            (101, 100, 8),
            (437, 100, 8),
            (60, 25, 1),
        ] {
            let plan = chunk_plan(n, chunk, overlap);
            assert_eq!(plan.first().unwrap().1 .0, 0);
            assert_eq!(plan.last().unwrap().1 .1, n - 1);
            for ((s, e), (a, b)) in &plan {
                assert!(e - s < chunk, "a chunk is at most {chunk} points");
                assert!(s <= a && a <= b && b <= e, "own stretch inside its chunk");
            }
            for w in plan.windows(2) {
                assert_eq!(w[0].1 .1, w[1].1 .0, "own stretches meet");
                assert!(w[1].0 .0 <= w[0].0 .1, "chunks overlap");
            }
        }
        assert_eq!(chunk_plan(250, 100, 8).len(), 3);
    }

    /// A fake OSRM answer for a chunk: every point matched onto a line shifted
    /// 3 m north, except the points in `unmatched`.
    fn fake_answer(pts: &[(f64, f64)], unmatched: &[usize]) -> ChunkMatch {
        let geom: Vec<(f64, f64)> = pts.iter().map(|p| (p.0, p.1 + 3.0)).collect();
        ChunkMatch {
            tracepoints: (0..pts.len())
                .map(|i| (!unmatched.contains(&i)).then_some((0, geom[i])))
                .collect(),
            matchings: vec![geom],
        }
    }

    #[test]
    fn stitching_follows_the_matches_and_keeps_gps_where_there_are_none() {
        let path: Vec<(f64, f64)> = (0..250).map(|i| (i as f64 * 50.0, 0.0)).collect();
        let plan = chunk_plan(path.len(), 100, 8);
        let answers: Vec<Option<ChunkMatch>> = plan
            .iter()
            .map(|((s, e), _)| Some(fake_answer(&path[*s..=*e], &[])))
            .collect();
        let st = stitch(&path, &plan, &answers);
        let (line, matched, unmatched) = (st.line, st.matched_m, st.unmatched_m);
        assert_eq!(unmatched, 0.0);
        assert_eq!(st.detours, 0);
        assert!((matched - 249.0 * 50.0).abs() < 1.0, "{matched}");
        assert!(
            line.iter().all(|p| (p.1 - 3.0).abs() < 1e-9),
            "all on the matched line"
        );
        assert!(
            line.windows(2).all(|w| w[1].0 > w[0].0),
            "monotone, no doubling back at the seams"
        );

        // the middle chunk fails outright: its own stretch is the GPS path
        let mut answers2 = answers.clone();
        answers2[1] = None;
        let st = stitch(&path, &plan, &answers2);
        let (line, matched, unmatched) = (st.line, st.matched_m, st.unmatched_m);
        let (a, b) = plan[1].1;
        assert!(
            (unmatched - (b - a) as f64 * 50.0).abs() < 10.0,
            "{unmatched}"
        );
        assert!(matched > 0.0);
        assert!(line.iter().any(|p| p.1 == 0.0) && line.iter().any(|p| p.1 == 3.0));
        assert!(line.windows(2).all(|w| w[1].0 >= w[0].0));

        // a few points tidied away inside a matching are bridged along it
        let answers3: Vec<Option<ChunkMatch>> = plan
            .iter()
            .map(|((s, e), _)| Some(fake_answer(&path[*s..=*e], &[10, 11, 12])))
            .collect();
        assert_eq!(stitch(&path, &plan, &answers3).unmatched_m, 0.0);

        // a loop OSRM invented between two points 50 m apart is refused, and
        // that stretch keeps the GPS geometry
        let pts: Vec<(f64, f64)> = path[..60].to_vec();
        let mut geom: Vec<(f64, f64)> = Vec::new();
        let mut tracepoints = Vec::new();
        for (i, p) in pts.iter().enumerate() {
            if i == 30 {
                // out 500 m to a U-turn and back
                geom.extend([
                    (p.0 - 25.0, 3.0),
                    (p.0 - 25.0, 503.0),
                    (p.0 - 20.0, 503.0),
                    (p.0 - 20.0, 3.0),
                ]);
            }
            geom.push((p.0, p.1 + 3.0));
            tracepoints.push(Some((0, (p.0, p.1 + 3.0))));
        }
        let plan = chunk_plan(pts.len(), 100, 8);
        let st = stitch(
            &pts,
            &plan,
            &[Some(ChunkMatch {
                tracepoints,
                matchings: vec![geom],
            })],
        );
        assert_eq!(st.detours, 1);
        assert!(
            st.line.iter().all(|q| q.1 < 10.0),
            "the loop is not in the line"
        );
        assert!((st.unmatched_m - 50.0).abs() < 10.0, "{}", st.unmatched_m);
    }

    #[test]
    fn a_match_answer_is_read_point_by_point() {
        let geom = encode_polyline(&[(13.0, 80.2), (13.001, 80.2)]);
        let answer = json!({
            "code": "Ok",
            "matchings": [{"geometry": geom}],
            "tracepoints": [
                {"matchings_index": 0, "location": [80.2, 13.0]},
                null,
                {"matchings_index": 0, "location": [80.2, 13.001]},
            ]
        });
        let m = parse_match(&answer, 3).unwrap();
        assert_eq!(m.tracepoints[0], Some((0, (13.0, 80.2))));
        assert_eq!(m.tracepoints[1], None);
        assert!(parse_match(&json!({"code": "NoMatch"}), 3).is_none());
    }
}
