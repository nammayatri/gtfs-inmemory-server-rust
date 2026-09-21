//! A route's map line from GPS (docs/gtfs-editor.md section 17).
//!
//! `POST /feeds/{g}/routes/{route_id}/polyline:gps` reads the pings of the
//! buses that ran the route over the last `days` days, keeps the bus RUNS that
//! pass this route's stops in order, folds them into one consolidated path,
//! and snaps that path to roads with OSRM `/match`. It is the answer to
//! "OSRM could not route through these stops": where the stops are wrong or
//! the router's map disagrees with them, the buses still know the way.
//!
//! The ideas are nandi's `s0_gps_ingest.py`, ported, with one change of
//! emphasis learned the hard way: a route NUMBER (`21G`) covers both
//! directions and several variants, each its own route_id here, and the
//! operator-entered label is sometimes wrong. Averaging everything labelled
//! `21G` draws a path through none of its stops. So a run counts only if it
//! passes at least [`Params::stop_share`] of THIS route's served stops, within
//! [`Params::stop_radius_m`], in increasing sequence order - which rejects the
//! other direction (it meets the stops backwards), other variants (they miss
//! the stops of the stretch they do not share) and mislabelled buses at once.
//!
//! The pipeline:
//!
//!  1. **Bus-days** (one query over the window): which devices carried this
//!     route number, per day, with enough pings inside a box around the
//!     route's stops. The table's sort key is the timestamp alone, so the time
//!     bound is what makes this cheap; the route label and the box are the
//!     entity bound. At most `max_bus_days`, spread over the window.
//!  2. **Tracks** (one query per day): those devices' pings on that day, still
//!     inside the box, averaged to one point per `bucket_s` seconds, and
//!     packed one device-hour per row - ClickHouse does the thinning, and the
//!     row count stays small (some network paths stall above ~300 rows).
//!  3. **Runs**: clean (impossible jumps), split at feed gaps and terminal
//!     dwells, match the stops, keep the runs that pass enough of them in
//!     order, cut each from its first to its last matched stop.
//!  4. **Consolidate**: grid cells over every kept run, the mean point of each,
//!     ordered along a reference run (the one through the most-travelled
//!     cells), thin cells and off-corridor samples dropped, the direction the
//!     majority drove, simplified.
//!  5. **Snap**: OSRM `/match`, chunked; a stretch it cannot match keeps the
//!     GPS geometry (`services::osrm::match_path`).
//!
//! Nothing here writes anything: the answer is a proposal the dashboard puts
//! into a draft, like the OSRM one.

use crate::services::clickhouse_reader::{
    quote, ClickHouseError, ClickHouseReader, ClickHouseSettings,
};
use crate::services::osrm::{self, dist, seg_dist, MatchOptions, MatchQuality, Planar};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub const DEFAULT_TABLE: &str = "atlas_kafka.amnex_direct_data";
pub const DEFAULT_DAYS: u32 = 14;
pub const DEFAULT_MAX_BUS_DAYS: usize = 30;
pub const DEFAULT_PAGE_ROWS: usize = 100;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(55);

/// The table's columns. `long`, not `lon`; `deviceId` is the vehicle key
/// (`vehicleNumber` is empty throughout).
const COL_TIME: &str = "timestamp";
const COL_LAT: &str = "lat";
const COL_LON: &str = "long";
const COL_DEVICE: &str = "deviceId";
const COL_ROUTE: &str = "routeNumber";
/// Service days are Indian days: a bus's night is not split at 05:30.
const DAY_TZ: &str = "Asia/Kolkata";
const DAY_OFFSET_S: i64 = 5 * 3600 + 30 * 60;

/// The algorithm's knobs. The defaults are the contract (docs section 17).
#[derive(Debug, Clone)]
pub struct Params {
    /// Pings are averaged to one point per this many seconds, in ClickHouse.
    pub bucket_s: i64,
    /// A bus-day needs this many route-labelled pings in the box to be read.
    pub min_bus_day_pings: u32,
    /// Stop reading tracks after this many averaged points.
    pub max_points: usize,
    /// The box around the route's stops that pings must fall in.
    pub bbox_margin_m: f64,
    pub max_speed_kmh: f64,
    /// A run ends at a gap in the feed longer than this...
    pub gap_s: i64,
    /// ...or where the bus stays within `dwell_radius_m` for `dwell_s`.
    pub dwell_radius_m: f64,
    pub dwell_s: i64,
    pub run_min_points: usize,
    pub run_min_extent_m: f64,
    /// A stop is passed when the track comes this close to it.
    pub stop_radius_m: f64,
    /// A run is kept when it passes this share of the served stops, in order.
    pub stop_share: f64,
    /// Two stops matched in a row are at most this far apart in time.
    pub max_leg_s: i64,
    /// Fewer kept runs than this is not enough evidence.
    pub min_runs: usize,
    pub cell_m: f64,
    pub corridor_m: f64,
    pub modal_ratio: f64,
    pub ref_step_m: f64,
    pub simplify_m: f64,
    /// `stop_coverage` counts the stops within this distance of the line.
    pub coverage_m: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            bucket_s: 20,
            min_bus_day_pings: 120,
            max_points: 150_000,
            bbox_margin_m: 1_000.0,
            max_speed_kmh: 110.0,
            gap_s: 600,
            dwell_radius_m: 100.0,
            dwell_s: 300,
            run_min_points: 10,
            run_min_extent_m: 1_000.0,
            stop_radius_m: 60.0,
            stop_share: 0.7,
            max_leg_s: 1_800,
            min_runs: 3,
            cell_m: 30.0,
            corridor_m: 80.0,
            modal_ratio: 0.25,
            ref_step_m: 20.0,
            simplify_m: 5.0,
            coverage_m: 30.0,
        }
    }
}

// ---------------------------------------------------------------- inputs

/// A served stop of the route, in sequence order.
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    pub stop_id: String,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ping {
    /// Unix seconds.
    pub t: i64,
    pub lat: f64,
    pub lon: f64,
}

/// One bus's (averaged) pings on one day.
#[derive(Debug, Clone)]
pub struct Track {
    pub device: String,
    pub pings: Vec<Ping>,
}

/// A route's served stops from its detail (`service::route_detail` shape):
/// boarded rows with a position; markers, jump and hidden stops are not
/// places a bus is seen at.
pub fn served_stops(detail: &Value) -> Vec<Stop> {
    detail["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|r| {
                    r["stop_type"]
                        .as_str()
                        .is_some_and(|t| !super::validation::UNSERVED_TYPES.contains(&t))
                })
                .filter_map(|r| {
                    let (lat, lon) = (r["lat"].as_f64()?, r["lon"].as_f64()?);
                    (super::validation::valid_lat_lon(lat, lon) && (lat, lon) != (0.0, 0.0)).then(
                        || Stop {
                            stop_id: r["stop_id"].as_str().unwrap_or("").to_string(),
                            lat,
                            lon,
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- runs

/// Drop what cannot be a bus: bad coordinates and jumps that imply an
/// impossible speed. The jump test compares with the last *kept* point, so one
/// wild fix does not drag the anchor; after five rejections in a row the
/// anchor is reset, because by then the bus really is somewhere else.
pub fn clean(pings: &[Ping], max_speed_kmh: f64) -> Vec<Ping> {
    let mut sorted: Vec<Ping> = pings
        .iter()
        .copied()
        .filter(|p| super::validation::valid_lat_lon(p.lat, p.lon) && (p.lat, p.lon) != (0.0, 0.0))
        .collect();
    sorted.sort_by(|a, b| a.t.cmp(&b.t));
    sorted.dedup_by(|b, a| a.t == b.t);
    let mut kept: Vec<Ping> = Vec::with_capacity(sorted.len());
    let mut rejected = 0;
    for p in sorted {
        if let Some(a) = kept.last() {
            let d = super::validation::haversine_m(a.lat, a.lon, p.lat, p.lon);
            let dt = (p.t - a.t).max(1) as f64;
            if d / dt * 3.6 > max_speed_kmh && rejected < 5 {
                rejected += 1;
                continue;
            }
        }
        rejected = 0;
        kept.push(p);
    }
    kept
}

/// Points further than this from the line joining their neighbours, where the
/// track goes out and comes back, are GPS spikes.
const SPIKE_M: f64 = 80.0;

/// Which points of a track are not spikes. An averaged 20-second point
/// carrying one wild fix sits a few hundred metres off the road - well within
/// any speed limit, so [`clean`] keeps it - and comes straight back. One
/// point out and back, or two, is dropped; a corner is not (its neighbours
/// are far apart), and neither is a detour (it does not come straight back).
pub fn despike(xy: &[(f64, f64)]) -> Vec<bool> {
    let n = xy.len();
    let mut keep = vec![true; n];
    for _ in 0..2 {
        let idx: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
        let m = idx.len();
        for w in 1..m.saturating_sub(1) {
            let (a, b, c) = (xy[idx[w - 1]], xy[idx[w]], xy[idx[w + 1]]);
            if seg_dist(b, a, c).0 > SPIKE_M && dist(a, c) < 0.5 * (dist(a, b) + dist(b, c)) {
                keep[idx[w]] = false;
            }
        }
        let idx: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
        let m = idx.len();
        for w in 1..m.saturating_sub(2) {
            let (a, b, c, d) = (xy[idx[w - 1]], xy[idx[w]], xy[idx[w + 1]], xy[idx[w + 2]]);
            if keep[idx[w]]
                && seg_dist(b, a, d).0 > SPIKE_M
                && seg_dist(c, a, d).0 > SPIKE_M
                && dist(a, d) < 0.5 * (dist(a, b) + dist(b, c) + dist(c, d))
            {
                keep[idx[w]] = false;
                keep[idx[w + 1]] = false;
            }
        }
    }
    keep
}

/// A dwell's own spot: the points this close to where the bus stood.
const DWELL_CORE_M: f64 = 30.0;

/// Split a cleaned track into runs: at a gap in the feed, and at a dwell - the
/// bus standing within `dwell_radius_m` for `dwell_s`, which is what it does at
/// a terminus before running the other way. The dwell point closes one run and
/// opens the next. Stubs (too few points, too little extent) are dropped.
pub fn split_runs(pings: &[Ping], pl: &Planar, p: &Params) -> Vec<Vec<Ping>> {
    let xy: Vec<(f64, f64)> = pings.iter().map(|q| pl.xy((q.lat, q.lon))).collect();
    let mut runs: Vec<Vec<Ping>> = Vec::new();
    let mut cur: Vec<Ping> = Vec::new();
    let n = pings.len();
    let mut i = 0;
    while i < n {
        if let Some(last) = cur.last() {
            if pings[i].t - last.t > p.gap_s {
                runs.push(std::mem::take(&mut cur));
            }
        }
        let mut j = i;
        while j + 1 < n
            && dist(xy[j + 1], xy[i]) <= p.dwell_radius_m
            && pings[j + 1].t - pings[j].t <= p.gap_s
        {
            j += 1;
        }
        if pings[j].t - pings[i].t >= p.dwell_s {
            // where it stood: the middle of the dwell. The run arriving ends
            // where the bus first got there, and the next starts where it
            // last was - not at the edge of the dwell radius, which would
            // cut up to that much off each end
            let mut mid: Vec<(f64, f64)> = xy[i..=j].to_vec();
            mid.sort_by(|a, b| a.0.total_cmp(&b.0));
            let cx = mid[mid.len() / 2].0;
            mid.sort_by(|a, b| a.1.total_cmp(&b.1));
            let c = (cx, mid[mid.len() / 2].1);
            let near = |k: usize| dist(xy[k], c) <= DWELL_CORE_M;
            let arrive = (i..=j).find(|&k| near(k)).unwrap_or(i);
            let leave = (i..=j).rev().find(|&k| near(k)).unwrap_or(j);
            cur.extend_from_slice(&pings[i..=arrive]);
            runs.push(std::mem::take(&mut cur));
            cur.extend_from_slice(&pings[leave..=j]);
            i = j + 1;
            continue;
        }
        cur.push(pings[i]);
        i += 1;
    }
    runs.push(cur);
    runs.retain(|r| {
        r.len() >= p.run_min_points && {
            let o = pl.xy((r[0].lat, r[0].lon));
            r.iter()
                .any(|q| dist(o, pl.xy((q.lat, q.lon))) >= p.run_min_extent_m)
        }
    });
    runs
}

/// The track passing one stop: where along the run (metres), when, how close.
#[derive(Debug, Clone, Copy)]
struct Event {
    /// Segment index + fraction: where to cut.
    pos: f64,
    arc: f64,
    t: f64,
    stop: usize,
    d: f64,
}

/// Segments longer than this are a gap, not a road: a stop is only matched at
/// their ends.
const MAX_INTERPOLATE_M: f64 = 600.0;
/// Two close stops may be met a little out of order.
const ORDER_SLACK_M: f64 = 30.0;

fn stop_events(xy: &[(f64, f64)], ts: &[i64], stops: &[(f64, f64)], radius: f64) -> Vec<Event> {
    let n = xy.len();
    if n < 2 {
        return vec![];
    }
    let mut cum = vec![0.0; n];
    for k in 1..n {
        cum[k] = cum[k - 1] + dist(xy[k - 1], xy[k]);
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for &(x, y) in xy {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let mut out = Vec::new();
    for (j, &s) in stops.iter().enumerate() {
        if s.0 < x0 - radius || s.0 > x1 + radius || s.1 < y0 - radius || s.1 > y1 + radius {
            continue;
        }
        let mut visit: Option<Event> = None;
        for k in 0..n - 1 {
            let (a, b) = (xy[k], xy[k + 1]);
            let seg = cum[k + 1] - cum[k];
            let (d, f) = if seg <= MAX_INTERPOLATE_M {
                seg_dist(s, a, b)
            } else {
                let (da, db) = (dist(s, a), dist(s, b));
                if da <= db {
                    (da, 0.0)
                } else {
                    (db, 1.0)
                }
            };
            if d <= radius {
                let e = Event {
                    pos: k as f64 + f,
                    arc: cum[k] + f * seg,
                    t: ts[k] as f64 + f * (ts[k + 1] - ts[k]) as f64,
                    stop: j,
                    d,
                };
                visit = match visit {
                    Some(v) if v.d <= d => Some(v),
                    _ => Some(e),
                };
            } else if let Some(v) = visit.take() {
                out.push(v);
            }
        }
        if let Some(v) = visit {
            out.push(v);
        }
    }
    out
}

/// The longest chain of events with the stop index strictly increasing and
/// the track moving forward (up to [`ORDER_SLACK_M`] back, for close stops),
/// at most `max_leg_s` between two matched stops; among equally long chains
/// the one covering the shortest stretch of track. Indexes into `ev`.
fn best_chain(ev: &[Event], max_leg_s: f64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..ev.len()).collect();
    order.sort_by(|&a, &b| {
        ev[a]
            .stop
            .cmp(&ev[b].stop)
            .then(ev[a].arc.total_cmp(&ev[b].arc))
    });
    // per event: (length, arc where the chain starts, previous event)
    let mut best: Vec<(usize, f64, Option<usize>)> = vec![(0, 0.0, None); ev.len()];
    for (oi, &e) in order.iter().enumerate() {
        let mut here = (1, ev[e].arc, None);
        for &f in &order[..oi] {
            if ev[f].stop >= ev[e].stop
                || ev[f].arc > ev[e].arc + ORDER_SLACK_M
                || ev[e].t - ev[f].t > max_leg_s
                || ev[e].t < ev[f].t - 60.0
            {
                continue;
            }
            let (len, start, _) = best[f];
            if len + 1 > here.0 || (len + 1 == here.0 && start > here.1) {
                here = (len + 1, start, Some(f));
            }
        }
        best[e] = here;
    }
    let Some(end) = (0..ev.len()).max_by(|&a, &b| {
        best[a]
            .0
            .cmp(&best[b].0)
            .then((ev[b].arc - best[b].1).total_cmp(&(ev[a].arc - best[a].1)))
            .then(b.cmp(&a))
    }) else {
        return vec![];
    };
    let mut chain = vec![end];
    while let Some(prev) = best[*chain.last().expect("non-empty")].2 {
        chain.push(prev);
    }
    chain.reverse();
    chain
}

/// Every chain of at least `need` stops in one run: the best one, then the
/// best ones in the stretches before and after it.
fn chains(ev: &[Event], need: usize, max_leg_s: f64) -> Vec<Vec<Event>> {
    if ev.len() < need || need == 0 {
        return vec![];
    }
    let idx = best_chain(ev, max_leg_s);
    if idx.len() < need {
        return vec![];
    }
    let chain: Vec<Event> = idx.iter().map(|&i| ev[i]).collect();
    let lo = chain.iter().map(|e| e.arc).fold(f64::MAX, f64::min);
    let hi = chain.iter().map(|e| e.arc).fold(f64::MIN, f64::max);
    let before: Vec<Event> = ev.iter().copied().filter(|e| e.arc < lo).collect();
    let after: Vec<Event> = ev.iter().copied().filter(|e| e.arc > hi).collect();
    let mut out = chains(&before, need, max_leg_s);
    out.push(chain);
    out.extend(chains(&after, need, max_leg_s));
    out
}

fn point_at(xy: &[(f64, f64)], pos: f64) -> (f64, f64) {
    let k = (pos.floor() as usize).min(xy.len() - 1);
    let f = pos - k as f64;
    let (a, b) = (xy[k], xy[(k + 1).min(xy.len() - 1)]);
    (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f)
}

/// A kept run: planar points from its first matched stop to its last.
#[derive(Debug, Clone)]
pub struct KeptRun {
    pub device: String,
    pub xy: Vec<(f64, f64)>,
    pub first_stop: usize,
    pub stops_matched: usize,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct RunCounts {
    pub runs_seen: usize,
    pub runs_used: usize,
}

/// Steps 3 of the pipeline: the runs of these tracks that pass the stops.
pub fn select_runs(
    tracks: &[Track],
    stops: &[Stop],
    pl: &Planar,
    p: &Params,
) -> (Vec<KeptRun>, RunCounts) {
    let stops_xy: Vec<(f64, f64)> = stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
    let need = ((stops.len() as f64 * p.stop_share).ceil() as usize).max(2);
    let mut kept = Vec::new();
    let mut counts = RunCounts::default();
    for track in tracks {
        let cleaned = clean(&track.pings, p.max_speed_kmh);
        for run in split_runs(&cleaned, pl, p) {
            let all: Vec<(f64, f64)> = run.iter().map(|q| pl.xy((q.lat, q.lon))).collect();
            let run: Vec<Ping> = run
                .iter()
                .zip(despike(&all))
                .filter_map(|(q, k)| k.then_some(*q))
                .collect();
            let xy: Vec<(f64, f64)> = run.iter().map(|q| pl.xy((q.lat, q.lon))).collect();
            let ts: Vec<i64> = run.iter().map(|q| q.t).collect();
            let ev = stop_events(&xy, &ts, &stops_xy, p.stop_radius_m);
            let found = chains(&ev, need, p.max_leg_s as f64);
            counts.runs_seen += found.len().max(1);
            for chain in found {
                let (lo, hi) = chain.iter().fold((f64::MAX, f64::MIN), |(lo, hi), e| {
                    (lo.min(e.pos), hi.max(e.pos))
                });
                let mut cut = vec![point_at(&xy, lo)];
                let first = (lo.floor() as usize) + 1;
                let last = hi.floor() as usize;
                if first <= last {
                    cut.extend_from_slice(&xy[first..=last]);
                }
                cut.push(point_at(&xy, hi));
                cut.dedup_by(|b, a| dist(*a, *b) < 0.01);
                if cut.len() < 2 {
                    continue;
                }
                kept.push(KeptRun {
                    device: track.device.clone(),
                    xy: cut,
                    first_stop: chain[0].stop,
                    stops_matched: chain.len(),
                });
            }
        }
    }
    counts.runs_used = kept.len();
    (kept, counts)
}

// ---------------------------------------------------------------- consolidate

/// Points every `step` along a run, each marked whether it is a position the
/// bus reported (true) or one filled in between two (false). A gap longer
/// than [`MAX_INTERPOLATE_M`] is not filled: a straight line across it would
/// be a guess.
fn densify(xy: &[(f64, f64)], step: f64) -> Vec<((f64, f64), bool)> {
    let mut out = Vec::with_capacity(xy.len() * 4);
    for w in xy.windows(2) {
        let (a, b) = (w[0], w[1]);
        out.push((a, true));
        let d = dist(a, b);
        if d <= MAX_INTERPOLATE_M && d > step {
            let k = (d / step).ceil() as usize;
            for i in 1..k {
                let f = i as f64 / k as f64;
                out.push(((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f), false));
            }
        }
    }
    if let Some(&last) = xy.last() {
        out.push((last, true));
    }
    out
}

/// A reported position outweighs the points filled in beside it: between two
/// pings 100 m apart the filled-in chord cuts every corner, and the ping is
/// where the bus actually was.
const REPORTED_WEIGHT: f64 = 5.0;

fn thin(xy: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
    let mut out = vec![xy[0]];
    for &q in &xy[1..] {
        if dist(*out.last().expect("non-empty"), q) >= step {
            out.push(q);
        }
    }
    if out.last() != xy.last() {
        out.push(*xy.last().expect("non-empty"));
    }
    out
}

fn cell_of(q: (f64, f64), cell: f64) -> (i64, i64) {
    ((q.0 / cell).floor() as i64, (q.1 / cell).floor() as i64)
}

/// The reference track as segments with their cumulative arc length.
struct Reference {
    xy: Vec<(f64, f64)>,
    cum: Vec<f64>,
}

impl Reference {
    fn new(xy: Vec<(f64, f64)>) -> Self {
        let mut cum = vec![0.0; xy.len()];
        for k in 1..xy.len() {
            cum[k] = cum[k - 1] + dist(xy[k - 1], xy[k]);
        }
        Self { xy, cum }
    }

    fn length(&self) -> f64 {
        *self.cum.last().unwrap_or(&0.0)
    }

    /// The nearest point of the reference to `q` among the segments whose arc
    /// range meets [lo, hi]: (arc, distance).
    fn nearest(&self, q: (f64, f64), lo: f64, hi: f64) -> Option<(f64, f64)> {
        let n = self.xy.len();
        let start = self.cum.partition_point(|&c| c < lo).saturating_sub(1);
        let mut best: Option<(f64, f64)> = None;
        for k in start..n.saturating_sub(1) {
            if self.cum[k] > hi {
                break;
            }
            let (d, t) = seg_dist(q, self.xy[k], self.xy[k + 1]);
            if best.map_or(true, |(_, bd)| d < bd) {
                best = Some((self.cum[k] + t * (self.cum[k + 1] - self.cum[k]), d));
            }
        }
        best
    }

    /// The first stretch of the reference within `tol` of `q`: (arc, distance).
    fn earliest(&self, q: (f64, f64), tol: f64) -> Option<(f64, f64)> {
        let mut found: Option<(f64, f64)> = None;
        for k in 0..self.xy.len().saturating_sub(1) {
            let (d, t) = seg_dist(q, self.xy[k], self.xy[k + 1]);
            match found {
                None if d <= tol => {
                    found = Some((self.cum[k] + t * (self.cum[k + 1] - self.cum[k]), d))
                }
                // keep following while it gets closer
                Some((_, fd)) if d < fd => {
                    found = Some((self.cum[k] + t * (self.cum[k + 1] - self.cum[k]), d))
                }
                Some(_) if d > tol => break,
                _ => {}
            }
        }
        found
    }
}

/// Douglas-Peucker, iterative.
pub fn simplify(xy: &[(f64, f64)], tol: f64) -> Vec<(f64, f64)> {
    if xy.len() < 3 {
        return xy.to_vec();
    }
    let mut keep = vec![false; xy.len()];
    keep[0] = true;
    keep[xy.len() - 1] = true;
    let mut stack = vec![(0, xy.len() - 1)];
    while let Some((lo, hi)) = stack.pop() {
        if hi <= lo + 1 {
            continue;
        }
        let (mut worst, mut at) = (-1.0, lo);
        for i in lo + 1..hi {
            let (d, _) = seg_dist(xy[i], xy[lo], xy[hi]);
            if d > worst {
                worst = d;
                at = i;
            }
        }
        if worst > tol {
            keep[at] = true;
            stack.push((lo, at));
            stack.push((at, hi));
        }
    }
    xy.iter()
        .zip(keep)
        .filter_map(|(q, k)| k.then_some(*q))
        .collect()
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Consolidation {
    pub cells: usize,
    pub cells_kept: usize,
    pub samples_off_corridor: usize,
    pub degraded: bool,
    pub runs_same_direction: usize,
    pub runs_opposite_direction: usize,
    pub reference_device: String,
}

/// Step 4: one path (planar, in the direction the runs drove) out of many.
pub fn consolidate(
    runs: &[KeptRun],
    stops: &[(f64, f64)],
    p: &Params,
) -> Option<(Vec<(f64, f64)>, Consolidation)> {
    if runs.is_empty() {
        return None;
    }
    let step = p.cell_m / 2.0;
    let marked: Vec<Vec<((f64, f64), bool)>> = runs.iter().map(|r| densify(&r.xy, step)).collect();
    let dense: Vec<Vec<(f64, f64)>> = marked
        .iter()
        .map(|m| m.iter().map(|(q, _)| *q).collect())
        .collect();

    // how many runs go through each cell
    let mut cell_runs: BTreeMap<(i64, i64), BTreeSet<usize>> = BTreeMap::new();
    for (ri, pts) in dense.iter().enumerate() {
        for &q in pts {
            cell_runs
                .entry(cell_of(q, p.cell_m))
                .or_default()
                .insert(ri);
        }
    }
    // the reference: the run through the most-travelled cells (log-weighted,
    // distinct cells, so a bus stuck in traffic does not win)
    let score = |ri: usize| -> f64 {
        dense[ri]
            .iter()
            .map(|&q| cell_of(q, p.cell_m))
            .collect::<BTreeSet<_>>()
            .iter()
            .map(|c| (cell_runs[c].len() as f64).ln_1p())
            .sum()
    };
    let ref_i = (0..runs.len())
        .map(|ri| (ri, score(ri)))
        .fold(
            (0, f64::MIN),
            |best, (ri, s)| if s > best.1 { (ri, s) } else { best },
        )
        .0;
    let reference = Reference::new(thin(&dense[ref_i], p.ref_step_m));
    if reference.xy.len() < 2 || reference.length() <= 0.0 {
        return None;
    }

    // where each stop sits along the reference, in order: a run's first
    // sample is searched for near its first matched stop, which is what keeps
    // a route that ends where it began from folding onto itself
    let mut stop_arc: Vec<Option<f64>> = Vec::with_capacity(stops.len());
    let mut last = 0.0;
    for &s in stops {
        let hit = if stop_arc.iter().all(Option::is_none) {
            reference.earliest(s, p.corridor_m)
        } else {
            reference
                .nearest(s, last - 50.0, f64::MAX)
                .filter(|(_, d)| *d <= p.corridor_m)
        };
        if let Some((a, _)) = hit {
            last = a;
        }
        stop_arc.push(hit.map(|(a, _)| a));
    }

    // every sample of every run, placed along the reference, monotonically
    struct Sample {
        q: (f64, f64),
        arc: f64,
        run: usize,
        w: f64,
    }
    let mut samples: Vec<Sample> = Vec::new();
    let mut off = 0;
    for (ri, pts) in dense.iter().enumerate() {
        let anchor = stop_arc
            .get(runs[ri].first_stop)
            .copied()
            .flatten()
            .or_else(|| {
                stop_arc[..runs[ri].first_stop.min(stop_arc.len())]
                    .iter()
                    .rev()
                    .flatten()
                    .next()
                    .copied()
            });
        let mut prev: Option<f64> = None;
        let mut travelled = 0.0;
        for (k, &q) in pts.iter().enumerate() {
            if k > 0 {
                travelled += dist(pts[k - 1], q);
            }
            let hit = match (prev, anchor) {
                (Some(s), _) => {
                    // near where the last sample was; and when that is not
                    // close, a little further on too - past a detour the
                    // reference run took and this one did not
                    let near = reference.nearest(q, s - 50.0, s + 2.0 * travelled + 300.0);
                    match near {
                        Some((_, d)) if d <= 20.0 => near,
                        _ => {
                            let far = reference.nearest(q, s - 50.0, s + 2.0 * travelled + 3_000.0);
                            match (near, far) {
                                (Some(n), Some(f)) if f.1 < n.1 * 0.5 => Some(f),
                                (None, f) => f,
                                (n, _) => n,
                            }
                        }
                    }
                }
                (None, Some(a)) => reference
                    .nearest(q, a - 150.0, a + 1_500.0 + 2.0 * travelled)
                    .filter(|(_, d)| *d <= p.corridor_m)
                    .or_else(|| reference.earliest(q, p.corridor_m)),
                (None, None) => reference.earliest(q, p.corridor_m),
            };
            match hit {
                Some((arc, d)) if d <= p.corridor_m => {
                    let w = if marked[ri][k].1 {
                        REPORTED_WEIGHT
                    } else {
                        1.0
                    };
                    samples.push(Sample { q, arc, run: ri, w });
                    prev = Some(arc);
                    travelled = 0.0;
                }
                _ => off += 1,
            }
        }
    }

    // cells, and within a cell each separate pass (a road driven twice, out
    // and back, is two passes of the same cells)
    let mut by_cell: BTreeMap<(i64, i64), Vec<usize>> = BTreeMap::new();
    for (i, s) in samples.iter().enumerate() {
        by_cell.entry(cell_of(s.q, p.cell_m)).or_default().push(i);
    }
    struct Pass {
        arc: f64,
        x: f64,
        y: f64,
        w: f64,
        runs: usize,
    }
    let mut passes: Vec<Pass> = Vec::new();
    for idx in by_cell.values() {
        let mut idx = idx.clone();
        idx.sort_by(|&a, &b| samples[a].arc.total_cmp(&samples[b].arc).then(a.cmp(&b)));
        let mut group: Vec<usize> = Vec::new();
        let flush = |group: &mut Vec<usize>, passes: &mut Vec<Pass>| {
            if group.is_empty() {
                return;
            }
            let n = group.len();
            let (sx, sy, sw) = group.iter().fold((0.0, 0.0, 0.0), |(sx, sy, sw), &i| {
                let s = &samples[i];
                (sx + s.q.0 * s.w, sy + s.q.1 * s.w, sw + s.w)
            });
            let runs: BTreeSet<usize> = group.iter().map(|&i| samples[i].run).collect();
            passes.push(Pass {
                arc: samples[group[n / 2]].arc,
                x: sx / sw,
                y: sy / sw,
                w: sw,
                runs: runs.len(),
            });
            group.clear();
        };
        for i in idx {
            if let Some(&g) = group.last() {
                if samples[i].arc - samples[g].arc > 250.0 {
                    flush(&mut group, &mut passes);
                }
            }
            group.push(i);
        }
        flush(&mut group, &mut passes);
    }
    let total_passes = passes.len();

    // keep the passes the corridor is made of: a detour one bus took is an
    // order of magnitude thinner than the road they all drove
    let mut counts: Vec<usize> = passes.iter().map(|c| c.runs).collect();
    counts.sort_unstable();
    let median = counts.get(counts.len() / 2).copied().unwrap_or(0);
    let floor = if runs.len() >= 4 { 2 } else { 1 };
    let mut threshold = floor.max((p.modal_ratio * median as f64).ceil() as usize);
    let mut degraded = false;
    if passes.iter().filter(|c| c.runs >= threshold).count() < 10 {
        // too few left to be a corridor: one honest thin line beats none
        threshold = 1;
        degraded = true;
    }
    let mut kept: Vec<&Pass> = passes.iter().filter(|c| c.runs >= threshold).collect();
    kept.sort_by(|a, b| a.arc.total_cmp(&b.arc));

    // passes side by side at one arc length are the two carriageways of one
    // road: merge them so the line does not zig-zag across the median
    let mut line: Vec<(f64, f64)> = Vec::new();
    let mut bucket: Vec<&Pass> = Vec::new();
    let flush = |bucket: &mut Vec<&Pass>, line: &mut Vec<(f64, f64)>| {
        if bucket.is_empty() {
            return;
        }
        let w: f64 = bucket.iter().map(|c| c.w).sum();
        line.push((
            bucket.iter().map(|c| c.x * c.w).sum::<f64>() / w,
            bucket.iter().map(|c| c.y * c.w).sum::<f64>() / w,
        ));
        bucket.clear();
    };
    for &c in &kept {
        if let Some(first) = bucket.first() {
            if c.arc - first.arc > p.cell_m * 0.5 {
                flush(&mut bucket, &mut line);
            }
        }
        bucket.push(c);
    }
    flush(&mut bucket, &mut line);
    if line.len() < 2 {
        return None;
    }

    // the direction most runs drove, by their ends (a bent corridor defeats
    // bearings; a horseshoe does not defeat "where did it start"). Every run
    // kept met the stops in order, so this is a check rather than a choice:
    // the line is oriented by the stops, first to last, which also holds on a
    // route that ends where it began - where comparing ends is a coin toss.
    let (rs, re) = (reference.xy[0], *reference.xy.last().expect("non-empty"));
    let (mut same, mut opposite) = (0, 0);
    for r in runs {
        let (s, e) = (r.xy[0], *r.xy.last().expect("non-empty"));
        if dist(s, rs) + dist(e, re) <= dist(s, re) + dist(e, rs) {
            same += 1;
        } else {
            opposite += 1;
        }
    }
    let placed: Vec<f64> = stop_arc.iter().flatten().copied().collect();
    let backwards = match (placed.first(), placed.last()) {
        (Some(a), Some(b)) if placed.len() >= 2 => a > b,
        _ => opposite > same,
    };
    if backwards {
        line.reverse();
    }
    let line = simplify(&line, p.simplify_m);
    Some((
        line,
        Consolidation {
            cells: total_passes,
            cells_kept: kept.len(),
            samples_off_corridor: off,
            degraded,
            runs_same_direction: same,
            runs_opposite_direction: opposite,
            reference_device: runs[ref_i].device.clone(),
        },
    ))
}

/// Share of the stops within `tol` metres of the line.
pub fn stop_coverage(line: &[(f64, f64)], stops: &[(f64, f64)], tol: f64) -> f64 {
    if stops.is_empty() || line.len() < 2 {
        return 0.0;
    }
    let hit = stops
        .iter()
        .filter(|&&s| line.windows(2).any(|w| seg_dist(s, w[0], w[1]).0 <= tol))
        .count();
    hit as f64 / stops.len() as f64
}

/// What the pure half of the pipeline produced.
#[derive(Debug, Clone)]
pub struct Built {
    /// (lat, lon), in the route's direction.
    pub line: Vec<(f64, f64)>,
    pub counts: RunCounts,
    pub buses: usize,
    pub consolidation: Consolidation,
}

/// Steps 3 and 4 over tracks already read: no database, no network. `Err`
/// carries the counts when there is not enough evidence.
pub fn build_line(tracks: &[Track], stops: &[Stop], p: &Params) -> Result<Built, RunCounts> {
    let Some(first) = stops.first() else {
        return Err(RunCounts::default());
    };
    let pl = Planar::around(first.lat, first.lon);
    let (runs, counts) = select_runs(tracks, stops, &pl, p);
    if runs.len() < p.min_runs {
        return Err(counts);
    }
    let stops_xy: Vec<(f64, f64)> = stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
    let Some((line, consolidation)) = consolidate(&runs, &stops_xy, p) else {
        return Err(counts);
    };
    let buses = runs
        .iter()
        .map(|r| r.device.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    Ok(Built {
        line: line
            .into_iter()
            .map(|q| {
                let (lat, lon) = pl.ll(q);
                ((lat * 1e6).round() / 1e6, (lon * 1e6).round() / 1e6)
            })
            .collect(),
        counts,
        buses,
        consolidation,
    })
}

// ---------------------------------------------------------------- queries

/// The route number as the pings carry it: trimmed, lower case. None when it
/// cannot be one (empty, too long, or holding a character the reader refuses).
pub fn route_label(short_name: &str) -> Option<String> {
    let t = short_name.trim();
    (!t.is_empty() && t.chars().count() <= 32 && !t.contains(';') && !t.contains('\n'))
        .then(|| t.to_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bbox {
    pub min_lat: f64,
    pub min_lon: f64,
    pub max_lat: f64,
    pub max_lon: f64,
}

impl Bbox {
    pub fn around(stops: &[Stop], margin_m: f64) -> Option<Self> {
        let first = stops.first()?;
        let mut b = Bbox {
            min_lat: first.lat,
            min_lon: first.lon,
            max_lat: first.lat,
            max_lon: first.lon,
        };
        for s in stops {
            b.min_lat = b.min_lat.min(s.lat);
            b.min_lon = b.min_lon.min(s.lon);
            b.max_lat = b.max_lat.max(s.lat);
            b.max_lon = b.max_lon.max(s.lon);
        }
        let dlat = margin_m / 110_574.0;
        let dlon = margin_m / (111_320.0 * ((b.min_lat + b.max_lat) / 2.0).to_radians().cos());
        Some(Bbox {
            min_lat: b.min_lat - dlat,
            min_lon: b.min_lon - dlon,
            max_lat: b.max_lat + dlat,
            max_lon: b.max_lon + dlon,
        })
    }

    fn sql(&self) -> String {
        format!(
            "{COL_LAT} BETWEEN {:.6} AND {:.6} AND {COL_LON} BETWEEN {:.6} AND {:.6}",
            self.min_lat, self.max_lat, self.min_lon, self.max_lon
        )
    }
}

fn label_expr() -> String {
    format!("lowerUTF8(trimBoth(ifNull(toString({COL_ROUTE}), '')))")
}

/// Step 1: bus-days that carried the route number, busiest first, at most
/// `per_day` a day, with the first and last ping that carried it - step 2
/// reads only around that span. No outer LIMIT: the caller pages it.
pub fn bus_days_sql(
    table: &str,
    from: i64,
    to: i64,
    label: &str,
    bbox: &Bbox,
    min_pings: u32,
    per_day: usize,
) -> String {
    format!(
        "SELECT toString({COL_DEVICE}) AS device, toString(toDate({COL_TIME}, '{DAY_TZ}')) AS day, count() AS n, \
         toUnixTimestamp(min({COL_TIME})) AS first_seen, toUnixTimestamp(max({COL_TIME})) AS last_seen \
         FROM {table} \
         WHERE {COL_TIME} >= toDateTime({from}) AND {COL_TIME} < toDateTime({to}) AND {COL_TIME} <= now() \
         AND {label_expr} = {label} AND {bbox} AND toString({COL_DEVICE}) != '' \
         GROUP BY device, day HAVING n >= {min_pings} \
         ORDER BY day DESC, n DESC, device \
         LIMIT {per_day} BY day",
        label_expr = label_expr(),
        label = quote(label),
        bbox = bbox.sql(),
    )
}

/// Step 2: some devices' pings over one stretch of time, averaged to one point
/// per `bucket_s`, packed one device-hour per row as `t,lat,lon|t,lat,lon|...`.
/// Pings labelled with another route are left out; unlabelled ones stay (a
/// quarter of pings carry no label). No outer LIMIT: the caller pages it.
pub fn tracks_sql(
    table: &str,
    from: i64,
    to: i64,
    devices: &[String],
    label: &str,
    bbox: &Bbox,
    bucket_s: i64,
) -> String {
    let list = devices
        .iter()
        .map(|d| quote(d))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT device, intDiv(t, 3600) AS h, count() AS points, sum(n) AS pings, \
         arrayStringConcat(groupArray(concat(toString(t), ',', toString(la), ',', toString(lo))), '|') AS track \
         FROM (\
         SELECT toString({COL_DEVICE}) AS device, intDiv(toUnixTimestamp({COL_TIME}), {bucket_s}) * {bucket_s} AS t, \
         round(avg({COL_LAT}), 6) AS la, round(avg({COL_LON}), 6) AS lo, count() AS n \
         FROM {table} \
         WHERE {COL_TIME} >= toDateTime({from}) AND {COL_TIME} < toDateTime({to}) AND {COL_TIME} <= now() \
         AND toString({COL_DEVICE}) IN ({list}) \
         AND {label_expr} IN ({label}, '') AND {bbox} \
         GROUP BY device, t) \
         GROUP BY device, h \
         ORDER BY device, h",
        label_expr = label_expr(),
        label = quote(label),
        bbox = bbox.sql(),
    )
}

/// Parse one packed row's track.
pub fn parse_packed(track: &str) -> Vec<Ping> {
    track
        .split('|')
        .filter_map(|p| {
            let mut it = p.split(',');
            let t = it.next()?.trim().parse::<i64>().ok()?;
            let lat = it.next()?.trim().parse::<f64>().ok()?;
            let lon = it.next()?.trim().parse::<f64>().ok()?;
            Some(Ping { t, lat, lon })
        })
        .collect()
}

/// At most `max` bus-days, spread over the days: the busiest of each day in
/// turn, newest day first. `rows` are (device, day, pings).
pub fn spread_bus_days(rows: &[(String, String, u64)], max: usize) -> Vec<(String, String)> {
    let mut by_day: BTreeMap<&str, Vec<(&str, u64)>> = BTreeMap::new();
    for (device, day, n) in rows {
        by_day.entry(day).or_default().push((device, *n));
    }
    for v in by_day.values_mut() {
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        v.dedup_by(|b, a| a.0 == b.0);
    }
    let days: Vec<&str> = by_day.keys().rev().copied().collect();
    let mut out = Vec::new();
    let mut round = 0;
    while out.len() < max {
        let mut any = false;
        for d in &days {
            if let Some((device, _)) = by_day[d].get(round) {
                any = true;
                if out.len() < max {
                    out.push((device.to_string(), d.to_string()));
                }
            }
        }
        if !any {
            break;
        }
        round += 1;
    }
    out
}

/// Pings read before a bus-day's first labelled ping and after its last.
const SPAN_MARGIN_S: i64 = 45 * 60;

/// The unix second an Indian service day starts at.
fn day_start(day: chrono::NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
        .timestamp()
        - DAY_OFFSET_S
}

fn today(now: i64) -> chrono::NaiveDate {
    chrono::DateTime::from_timestamp(now + DAY_OFFSET_S, 0)
        .expect("valid time")
        .date_naive()
}

// ---------------------------------------------------------------- service

#[derive(Debug, Clone)]
pub struct GpsLineSettings {
    pub clickhouse: ClickHouseSettings,
    /// `database.table`.
    pub table: String,
    pub days: u32,
    /// The feeds whose routes the pings describe.
    pub feeds: Vec<String>,
    pub max_bus_days: usize,
    /// Rows per ClickHouse answer.
    pub page_rows: usize,
    /// The whole suggestion - waiting for another one, the queries, OSRM.
    pub timeout: Duration,
    pub params: Params,
}

/// The settings the dhall block describes, defaults filled in.
pub fn settings_from_config(
    c: &crate::environment::GtfsGpsConfig,
    password: Option<String>,
) -> GpsLineSettings {
    let timeout = Duration::from_secs(u64::from(c.timeout_seconds.unwrap_or(55).clamp(5, 300)));
    GpsLineSettings {
        clickhouse: ClickHouseSettings {
            url: c.url.clone(),
            user: c.user.clone(),
            password,
            query_timeout: Duration::from_secs(30).min(timeout),
            min_gap: crate::services::clickhouse_reader::DEFAULT_MIN_GAP,
        },
        table: c
            .table
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TABLE.to_string()),
        days: c.days.unwrap_or(DEFAULT_DAYS),
        feeds: c
            .feeds
            .clone()
            .unwrap_or_else(|| vec!["chennai_bus".to_string()]),
        max_bus_days: c
            .max_bus_days
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_BUS_DAYS)
            .clamp(1, 200),
        page_rows: c
            .page_rows
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_PAGE_ROWS)
            .clamp(10, 10_000),
        timeout,
        params: Params::default(),
    }
}

/// Why no line.
#[derive(Debug)]
pub enum GpsFailure {
    /// The route has no usable number to look the pings up by.
    NoRouteNumber,
    /// Fewer than two served stops with a position.
    NotEnoughStops(usize),
    /// Too little evidence, with the counts.
    NotEnoughRuns(Value),
    Timeout,
    Query(String),
    Internal(String),
}

impl From<ClickHouseError> for GpsFailure {
    fn from(e: ClickHouseError) -> Self {
        match e {
            ClickHouseError::Timeout(_) => GpsFailure::Timeout,
            ClickHouseError::ReadOnly(why) => GpsFailure::Internal(why),
            other => GpsFailure::Query(other.to_string()),
        }
    }
}

/// The part read from ClickHouse, kept per day so a second click is free.
#[derive(Debug, Clone)]
enum Stage {
    Line {
        line: Vec<(f64, f64)>,
        evidence: Value,
    },
    NotEnough(Value),
}

struct CacheEntry {
    at: Instant,
    stage: Option<Stage>,
    answer: Option<Value>,
}

const CACHE_ENTRIES: usize = 256;

pub struct GpsLine {
    pub settings: GpsLineSettings,
    reader: ClickHouseReader,
    http: reqwest::Client,
    /// One suggestion at a time per pod: the cluster is shared.
    permit: Semaphore,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl std::fmt::Debug for GpsLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpsLine")
            .field("settings", &self.settings)
            .finish()
    }
}

/// What a suggestion is asked for.
pub struct RouteQuery<'a> {
    pub gtfs_id: &'a str,
    pub route_id: &'a str,
    pub short_name: &'a str,
    pub stops: &'a [Stop],
}

fn valid_table(t: &str) -> bool {
    let parts: Vec<&str> = t.split('.').collect();
    (1..=2).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

impl GpsLine {
    pub fn new(settings: GpsLineSettings) -> Result<Self, String> {
        if !valid_table(&settings.table) {
            return Err(format!("{:?} is not a table name", settings.table));
        }
        if settings.days == 0 || settings.days > 60 {
            return Err("gps days must be 1 to 60".into());
        }
        let reader = ClickHouseReader::new(settings.clickhouse.clone())?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("cannot build the OSRM client: {}", e.without_url()))?;
        Ok(Self {
            settings,
            reader,
            http,
            permit: Semaphore::new(1),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn serves(&self, gtfs_id: &str) -> bool {
        self.settings.feeds.iter().any(|f| f == gtfs_id)
    }

    /// How many statements have been sent to ClickHouse.
    pub fn queries(&self) -> u64 {
        self.reader.queries()
    }

    fn cache_key(q: &RouteQuery, day: chrono::NaiveDate) -> String {
        let mut basis = format!("{}\n", q.short_name.trim());
        for s in q.stops {
            basis.push_str(&format!("{}|{:.6}|{:.6}\n", s.stop_id, s.lat, s.lon));
        }
        format!(
            "{}|{}|{}|{}",
            q.gtfs_id,
            q.route_id,
            super::crypto::sha256_hex(basis.as_bytes()),
            day
        )
    }

    fn cached(&self, key: &str) -> (Option<Value>, Option<Stage>) {
        let cache = self.cache.lock().expect("cache lock");
        match cache.get(key) {
            Some(e) => (e.answer.clone(), e.stage.clone()),
            None => (None, None),
        }
    }

    fn remember(&self, key: &str, stage: Option<Stage>, answer: Option<Value>) {
        let mut cache = self.cache.lock().expect("cache lock");
        if !cache.contains_key(key) && cache.len() >= CACHE_ENTRIES {
            if let Some(old) = cache
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone())
            {
                cache.remove(&old);
            }
        }
        let e = cache.entry(key.to_string()).or_insert(CacheEntry {
            at: Instant::now(),
            stage: None,
            answer: None,
        });
        e.at = Instant::now();
        if stage.is_some() {
            e.stage = stage;
        }
        if answer.is_some() {
            e.answer = answer;
        }
    }

    /// Suggest a line for a route. `osrm_base` is the OSRM server, if any.
    pub async fn suggest(
        &self,
        osrm_base: Option<&str>,
        q: RouteQuery<'_>,
    ) -> Result<Value, GpsFailure> {
        let label = route_label(q.short_name).ok_or(GpsFailure::NoRouteNumber)?;
        if q.stops.len() < 2 {
            return Err(GpsFailure::NotEnoughStops(q.stops.len()));
        }
        let now = chrono::Utc::now().timestamp();
        let key = Self::cache_key(&q, today(now));
        if let (Some(mut answer), _) = self.cached(&key) {
            answer["evidence"]["cached"] = json!(true);
            return Ok(answer);
        }
        let deadline = Instant::now() + self.settings.timeout;
        let work = async {
            let (line, evidence) = self.stage(&q, &label, &key, now).await?;
            // whoever held the permit before us may have answered this already
            if let (Some(mut answer), _) = self.cached(&key) {
                answer["evidence"]["cached"] = json!(true);
                return Ok(answer);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let answer = self
                .finish(
                    osrm_base,
                    &q,
                    &line,
                    evidence,
                    left.saturating_sub(Duration::from_secs(1)),
                )
                .await;
            // a line OSRM could not snap because it was down is not kept: the
            // next click tries OSRM again (the GPS half stays cached)
            let settled = answer["evidence"]["matched"] == json!(MatchQuality::Osrm)
                || osrm_base.map_or(true, |b| b.trim().is_empty());
            if settled {
                self.remember(&key, None, Some(answer.clone()));
            }
            Ok(answer)
        };
        match tokio::time::timeout_at(deadline.into(), work).await {
            Ok(r) => r,
            Err(_) => Err(GpsFailure::Timeout),
        }
    }

    /// Steps 1-4: the consolidated GPS path (lat, lon) and its evidence, from
    /// the cache or from ClickHouse, one suggestion at a time per pod.
    pub async fn gps_path(
        &self,
        q: &RouteQuery<'_>,
    ) -> Result<(Vec<(f64, f64)>, Value), GpsFailure> {
        let label = route_label(q.short_name).ok_or(GpsFailure::NoRouteNumber)?;
        if q.stops.len() < 2 {
            return Err(GpsFailure::NotEnoughStops(q.stops.len()));
        }
        let now = chrono::Utc::now().timestamp();
        let key = Self::cache_key(q, today(now));
        self.stage(q, &label, &key, now).await
    }

    async fn stage(
        &self,
        q: &RouteQuery<'_>,
        label: &str,
        key: &str,
        now: i64,
    ) -> Result<(Vec<(f64, f64)>, Value), GpsFailure> {
        let stage = {
            let _one_at_a_time = self
                .permit
                .acquire()
                .await
                .map_err(|_| GpsFailure::Internal("the GPS permit is closed".into()))?;
            match self.cached(key).1 {
                Some(stage) => stage,
                None => {
                    let stage = self.read_and_build(q, label, now).await?;
                    self.remember(key, Some(stage.clone()), None);
                    stage
                }
            }
        };
        match stage {
            Stage::NotEnough(counts) => Err(GpsFailure::NotEnoughRuns(counts)),
            Stage::Line { line, evidence } => Ok((line, evidence)),
        }
    }

    /// Step 5 and the answer: snap the path with OSRM (where it can), measure
    /// it against the stops.
    pub async fn finish(
        &self,
        osrm_base: Option<&str>,
        q: &RouteQuery<'_>,
        line: &[(f64, f64)],
        mut evidence: Value,
        budget: Duration,
    ) -> Value {
        let matched = osrm::match_path(
            &self.http,
            osrm_base,
            line,
            &MatchOptions::default(),
            budget,
        )
        .await;
        let pl = Planar::around(q.stops[0].lat, q.stops[0].lon);
        let line_xy: Vec<(f64, f64)> = matched.points.iter().map(|&p| pl.xy(p)).collect();
        let stops_xy: Vec<(f64, f64)> = q.stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
        let coverage = stop_coverage(&line_xy, &stops_xy, self.settings.params.coverage_m);
        evidence["stop_coverage"] = json!((coverage * 1000.0).round() / 1000.0);
        evidence["matched"] = json!(matched.quality);
        evidence["matched_share"] = json!((matched.matched_share * 1000.0).round() / 1000.0);
        if let Some(e) = &matched.error {
            evidence["osrm_error"] = json!(e);
        }
        evidence["osrm_detours_skipped"] = json!(matched.detours);
        evidence["points"] = json!(matched.points.len());
        evidence["length_m"] = json!(osrm::length_m(&matched.points).round());
        evidence["cached"] = json!(false);
        json!({
            "route_id": q.route_id,
            "encoded_polyline": osrm::encode_polyline(&matched.points),
            "polyline_source": "gps",
            "saved": false,
            "evidence": evidence,
        })
    }

    async fn read_and_build(
        &self,
        q: &RouteQuery<'_>,
        label: &str,
        now: i64,
    ) -> Result<Stage, GpsFailure> {
        let s = &self.settings;
        let p = &s.params;
        let bbox = Bbox::around(q.stops, p.bbox_margin_m)
            .ok_or(GpsFailure::NotEnoughStops(q.stops.len()))?;
        let last_day = today(now);
        let first_day = last_day - chrono::Duration::days(i64::from(s.days) - 1);
        let from = day_start(first_day);
        let queries_before = self.reader.queries();
        let mut evidence = json!({
            "from": first_day.to_string(),
            "to": last_day.to_string(),
            "days": s.days,
            "route_number": q.short_name.trim(),
            "stops": q.stops.len(),
        });

        // 1. bus-days
        let per_day = (s.max_bus_days + s.days as usize - 1) / (s.days as usize) + 1;
        let rows = self
            .reader
            .paged(
                &bus_days_sql(
                    &s.table,
                    from,
                    now,
                    label,
                    &bbox,
                    p.min_bus_day_pings,
                    per_day,
                ),
                s.page_rows,
                1_000,
            )
            .await?;
        let mut seen: HashMap<(String, String), (i64, i64)> = HashMap::new();
        let rows: Vec<(String, String, u64)> = rows
            .into_iter()
            .filter_map(|r| {
                let (device, day, n) = (r.first()?, r.get(1)?, r.get(2)?.parse::<u64>().ok()?);
                if device.is_empty() || device.contains(';') {
                    return None;
                }
                let span = (r.get(3)?.parse::<i64>().ok(), r.get(4)?.parse::<i64>().ok());
                if let (Some(a), Some(b)) = span {
                    seen.insert((device.clone(), day.clone()), (a, b));
                }
                Some((device.clone(), day.clone(), n))
            })
            .collect();
        let chosen = spread_bus_days(&rows, s.max_bus_days);

        // 2. their tracks, a day at a time: only around the hours the chosen
        // buses carried the route number (the sort key is the timestamp, so a
        // narrower span is fewer rows read), with room for a run that began
        // before its first labelled ping or ended after its last
        let mut by_day: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut span: HashMap<String, (i64, i64)> = HashMap::new();
        for (device, day) in &chosen {
            by_day.entry(day.clone()).or_default().push(device.clone());
            if let Some(&(a, b)) = seen.get(&(device.clone(), day.clone())) {
                let e = span.entry(day.clone()).or_insert((a, b));
                *e = (e.0.min(a), e.1.max(b));
            }
        }
        let mut tracks: BTreeMap<(String, String), Vec<Ping>> = BTreeMap::new();
        let (mut points, mut pings, mut truncated) = (0usize, 0u64, false);
        for (day, devices) in by_day.iter().rev() {
            if points >= p.max_points {
                truncated = true;
                break;
            }
            let Ok(date) = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d") else {
                continue;
            };
            let (mut a, mut b) = (
                day_start(date).max(from),
                (day_start(date) + 86_400).min(now),
            );
            if let Some(&(first, last)) = span.get(day) {
                a = a.max(first - SPAN_MARGIN_S);
                b = b.min(last + SPAN_MARGIN_S + 1);
            }
            if a >= b {
                continue;
            }
            let rows = self
                .reader
                .paged(
                    &tracks_sql(&s.table, a, b, devices, label, &bbox, p.bucket_s),
                    s.page_rows,
                    10_000,
                )
                .await?;
            for r in rows {
                let (Some(device), Some(n), Some(track)) = (r.first(), r.get(3), r.get(4)) else {
                    continue;
                };
                let parsed = parse_packed(track);
                points += parsed.len();
                pings += n.parse::<u64>().unwrap_or(0);
                tracks
                    .entry((device.clone(), day.clone()))
                    .or_default()
                    .extend(parsed);
            }
        }
        evidence["bus_days"] = json!(chosen.len());
        evidence["pings"] = json!(pings);
        evidence["points_read"] = json!(points);
        evidence["truncated"] = json!(truncated);
        evidence["queries"] = json!(self.reader.queries() - queries_before);

        // 3 and 4
        let tracks: Vec<Track> = tracks
            .into_iter()
            .map(|((device, _), pings)| Track { device, pings })
            .collect();
        match build_line(&tracks, q.stops, p) {
            Ok(built) => {
                evidence["buses"] = json!(built.buses);
                evidence["runs_seen"] = json!(built.counts.runs_seen);
                evidence["runs_used"] = json!(built.counts.runs_used);
                evidence["consolidation"] = json!({
                    "cells": built.consolidation.cells,
                    "cells_kept": built.consolidation.cells_kept,
                    "samples_off_corridor": built.consolidation.samples_off_corridor,
                    "degraded": built.consolidation.degraded,
                    "runs_same_direction": built.consolidation.runs_same_direction,
                    "runs_opposite_direction": built.consolidation.runs_opposite_direction,
                });
                Ok(Stage::Line {
                    line: built.line,
                    evidence,
                })
            }
            Err(counts) => {
                evidence["runs_seen"] = json!(counts.runs_seen);
                evidence["runs_used"] = json!(counts.runs_used);
                evidence["min_runs"] = json!(p.min_runs);
                evidence["buses"] = json!(tracks
                    .iter()
                    .map(|t| t.device.as_str())
                    .collect::<BTreeSet<_>>()
                    .len());
                Ok(Stage::NotEnough(evidence))
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    // ------------------------------------------------------------ synthetic data

    /// A small deterministic generator: no `rand` in the tree.
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self(
                seed.wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407),
            )
        }
        pub fn next_f64(&mut self) -> f64 {
            // xorshift64*
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
        }
        pub fn uniform(&mut self, a: f64, b: f64) -> f64 {
            a + (b - a) * self.next_f64()
        }
        pub fn gauss(&mut self, sigma: f64) -> f64 {
            let u1 = self.next_f64().max(1e-12);
            let u2 = self.next_f64();
            sigma * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    pub const ORIGIN: (f64, f64) = (13.0, 80.2);

    /// A 7.5 km corridor (planar metres): straight, a right angle, a long
    /// curve, a dog-leg - the shapes a chord between pings cuts.
    pub fn corridor() -> Vec<(f64, f64)> {
        let mut pts = vec![(0.0, 0.0)];
        let push_line = |pts: &mut Vec<(f64, f64)>, dx: f64, dy: f64, n: usize| {
            let (x0, y0) = *pts.last().unwrap();
            for i in 1..=n {
                let f = i as f64 / n as f64;
                pts.push((x0 + dx * f, y0 + dy * f));
            }
        };
        push_line(&mut pts, 2_000.0, 0.0, 40);
        push_line(&mut pts, 0.0, 1_200.0, 24);
        // a quarter circle of radius 400 m, turning right
        let (cx, cy) = (pts.last().unwrap().0 + 400.0, pts.last().unwrap().1);
        for i in 1..=30 {
            let a = std::f64::consts::PI - (i as f64 / 30.0) * std::f64::consts::FRAC_PI_2;
            pts.push((cx + 400.0 * a.cos(), cy + 400.0 * a.sin()));
        }
        push_line(&mut pts, 1_500.0, 0.0, 30);
        push_line(&mut pts, 300.0, -300.0, 8);
        push_line(&mut pts, 1_200.0, 0.0, 24);
        pts
    }

    pub fn resample_xy(xy: &[(f64, f64)], step: f64) -> Vec<(f64, f64)> {
        let mut out = vec![xy[0]];
        let mut carry = 0.0;
        for w in xy.windows(2) {
            let (a, b) = (w[0], w[1]);
            let seg = dist(a, b);
            let mut t = step - carry;
            while t <= seg {
                let f = t / seg;
                out.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
                t += step;
            }
            carry = seg - (t - step);
        }
        out.push(*xy.last().unwrap());
        out
    }

    /// A parallel line `off` metres to the left of travel.
    pub fn offset(xy: &[(f64, f64)], off: f64) -> Vec<(f64, f64)> {
        (0..xy.len())
            .map(|i| {
                let (a, b) = (xy[i.saturating_sub(1)], xy[(i + 1).min(xy.len() - 1)]);
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let n = (dx * dx + dy * dy).sqrt().max(1e-9);
                (xy[i].0 - dy / n * off, xy[i].1 + dx / n * off)
            })
            .collect()
    }

    pub fn to_ll(xy: (f64, f64)) -> (f64, f64) {
        Planar::new(ORIGIN.0, ORIGIN.1).ll(xy)
    }

    /// Stops every ~400 m on the left kerb (traffic keeps left), in order.
    pub fn stops_along(path: &[(f64, f64)], prefix: &str) -> Vec<Stop> {
        let dense = resample_xy(path, 5.0);
        let kerb = offset(&dense, 6.0);
        kerb.iter()
            .step_by(80)
            .chain(std::iter::once(kerb.last().unwrap()))
            .enumerate()
            .map(|(i, &q)| {
                let (lat, lon) = to_ll(q);
                Stop {
                    stop_id: format!("{prefix}{i}"),
                    lat,
                    lon,
                }
            })
            .collect()
    }

    /// A bus driving `path`: a ping every 10 s (averaged in 20 s buckets as
    /// ClickHouse would), 8 m of noise, the odd wild fix, dwells at stops.
    pub fn drive(path: &[(f64, f64)], rng: &mut Rng, t0: i64, outliers: f64) -> Vec<Ping> {
        let track = resample_xy(path, 5.0);
        let total = 5.0 * (track.len() - 1) as f64;
        let mut raw: Vec<(i64, (f64, f64))> = Vec::new();
        let (mut pos, mut t) = (0.0, t0);
        while pos < total {
            let i = ((pos / 5.0) as usize).min(track.len() - 2);
            let f = pos / 5.0 - i as f64;
            let (a, b) = (track[i], track[i + 1]);
            let mut q = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
            if rng.next_f64() < outliers {
                let ang = rng.uniform(0.0, std::f64::consts::TAU);
                let r = rng.uniform(300.0, 1_500.0);
                q = (q.0 + r * ang.cos(), q.1 + r * ang.sin());
            } else {
                q = (q.0 + rng.gauss(8.0), q.1 + rng.gauss(8.0));
            }
            raw.push((t, q));
            let speed = if rng.next_f64() < 0.12 {
                0.5
            } else {
                rng.uniform(4.0, 9.0)
            };
            pos += speed * 10.0;
            t += 10;
        }
        // the 20 s buckets ClickHouse averages to
        let mut buckets: BTreeMap<i64, Vec<(f64, f64)>> = BTreeMap::new();
        for (t, q) in raw {
            buckets.entry(t / 20 * 20).or_default().push(q);
        }
        buckets
            .into_iter()
            .map(|(t, qs)| {
                let n = qs.len() as f64;
                let (lat, lon) = to_ll((
                    qs.iter().map(|q| q.0).sum::<f64>() / n,
                    qs.iter().map(|q| q.1).sum::<f64>() / n,
                ));
                Ping { t, lat, lon }
            })
            .collect()
    }

    /// A bus standing still for `secs`.
    pub fn stand(at: (f64, f64), rng: &mut Rng, t0: i64, secs: i64) -> Vec<Ping> {
        (0..secs / 20)
            .map(|i| {
                let (lat, lon) = to_ll((at.0 + rng.gauss(4.0), at.1 + rng.gauss(4.0)));
                Ping {
                    t: t0 + i * 20,
                    lat,
                    lon,
                }
            })
            .collect()
    }

    /// A bus-day: the plan's legs in turn with a 12-minute dwell after each.
    pub fn bus_day(device: &str, legs: &[Vec<(f64, f64)>], rng: &mut Rng, t0: i64) -> Track {
        // out of the depot onto the first stand, as a bus's day starts
        let mut pings = stand(legs[0][0], rng, t0, 300);
        let mut t = pings.last().unwrap().t + 20;
        for leg in legs {
            let run = drive(leg, rng, t, 0.02);
            t = run.last().unwrap().t + 20;
            pings.extend(run);
            let end = *leg.last().unwrap();
            let dwell = stand(end, rng, t, 720);
            t = dwell.last().unwrap().t + 20;
            pings.extend(dwell);
        }
        Track {
            device: device.into(),
            pings,
        }
    }

    fn reversed(xy: &[(f64, f64)]) -> Vec<(f64, f64)> {
        xy.iter().rev().copied().collect()
    }

    /// Distances from each point of `line` to `path` (planar).
    fn distances(line: &[(f64, f64)], path: &[(f64, f64)]) -> Vec<f64> {
        let mut d: Vec<f64> = line
            .iter()
            .map(|&q| {
                path.windows(2)
                    .map(|w| seg_dist(q, w[0], w[1]).0)
                    .fold(f64::MAX, f64::min)
            })
            .collect();
        d.sort_by(f64::total_cmp);
        d
    }

    /// The fleet of the self-test: 10 runs of the route, 4 the other way, 2 of
    /// a variant that leaves the corridor half way, 2 of another route
    /// altogether that carried this route's label.
    pub fn fleet(truth: &[(f64, f64)], rng: &mut Rng) -> Vec<Track> {
        let back = offset(&reversed(truth), 0.0);
        // the variant: the first 40% of the corridor, then a parallel road
        // 700 m away
        let dense = resample_xy(truth, 5.0);
        let split = dense.len() * 2 / 5;
        let mut variant: Vec<(f64, f64)> = dense[..split].to_vec();
        let away = offset(&dense[split..], 700.0);
        variant.push(away[0]);
        variant.extend(away);
        let elsewhere: Vec<(f64, f64)> = truth.iter().map(|q| (q.0, q.1 - 2_500.0)).collect();
        let t0 = 1_758_000_000;
        vec![
            bus_day(
                "dev-a",
                &[truth.to_vec(), back.clone(), truth.to_vec(), back.clone()],
                rng,
                t0,
            ),
            bus_day(
                "dev-b",
                &[truth.to_vec(), back.clone(), truth.to_vec()],
                rng,
                t0 + 900,
            ),
            bus_day(
                "dev-c",
                &[truth.to_vec(), variant.clone(), truth.to_vec()],
                rng,
                t0 + 1_800,
            ),
            bus_day(
                "dev-d",
                &[truth.to_vec(), elsewhere.clone(), truth.to_vec()],
                rng,
                t0 + 2_700,
            ),
            bus_day(
                "dev-e",
                &[variant, back, truth.to_vec(), elsewhere],
                rng,
                t0 + 3_600,
            ),
        ]
    }

    #[test]
    fn run_selection_keeps_this_route_in_this_direction_only() {
        let truth = corridor();
        let stops = stops_along(&truth, "S");
        let mut rng = Rng::new(7);
        let tracks = fleet(&truth, &mut rng);
        let p = Params::default();
        let pl = Planar::around(stops[0].lat, stops[0].lon);
        let (runs, counts) = select_runs(&tracks, &stops, &pl, &p);
        // dev-a 2 + dev-b 2 + dev-c 2 + dev-d 2 + dev-e 1
        assert_eq!(counts.runs_used, 9, "{counts:?}");
        assert!(counts.runs_seen >= 17, "{counts:?}");
        assert_eq!(runs.len(), 9);
        for r in &runs {
            assert!(
                r.stops_matched * 10 >= stops.len() * 7,
                "{} of {}",
                r.stops_matched,
                stops.len()
            );
        }

        // the reverse route: its stops on the other kerb, in the other order -
        // the runs kept now are exactly the four that drove back
        let back_stops = stops_along(&reversed(&truth), "B");
        let (back_runs, _) = select_runs(&tracks, &back_stops, &pl, &p);
        assert_eq!(back_runs.len(), 4);

        // a variant sharing only the first 40%: none of the runs of the route
        // pass 70% of its stops, only the variant's own two
        let dense = resample_xy(&truth, 5.0);
        let split = dense.len() * 2 / 5;
        let mut variant: Vec<(f64, f64)> = dense[..split].to_vec();
        variant.extend(offset(&dense[split..], 700.0));
        let variant_stops = stops_along(&variant, "V");
        let (variant_runs, _) = select_runs(&tracks, &variant_stops, &pl, &p);
        assert_eq!(
            variant_runs.len(),
            2,
            "only dev-c's and dev-e's variant runs"
        );
        assert!(variant_runs
            .iter()
            .all(|r| r.device == "dev-c" || r.device == "dev-e"));
    }

    #[test]
    fn a_run_is_cut_to_the_stops_it_passed() {
        let truth = corridor();
        let stops = stops_along(&truth, "S");
        // the bus starts 800 m before the first stop and runs 800 m past the last
        let dense = resample_xy(&truth, 5.0);
        let (a, b) = (dense[0], dense[1]);
        let before = (a.0 - (b.0 - a.0) / dist(a, b) * 800.0, a.1);
        let (y, z) = (dense[dense.len() - 2], dense[dense.len() - 1]);
        let after = (
            z.0 + (z.0 - y.0) / dist(y, z) * 800.0,
            z.1 + (z.1 - y.1) / dist(y, z) * 800.0,
        );
        let mut longer = vec![before];
        longer.extend(truth.iter().copied());
        longer.push(after);
        let mut rng = Rng::new(3);
        let track = Track {
            device: "x".into(),
            pings: drive(&longer, &mut rng, 1_758_000_000, 0.0),
        };
        let pl = Planar::around(stops[0].lat, stops[0].lon);
        let (runs, _) = select_runs(&[track], &stops, &pl, &Params::default());
        assert_eq!(runs.len(), 1);
        let first = pl.xy((stops[0].lat, stops[0].lon));
        let last = pl.xy((stops.last().unwrap().lat, stops.last().unwrap().lon));
        let r = &runs[0];
        assert!(
            dist(r.xy[0], first) < 60.0,
            "starts at the first stop: {}",
            dist(r.xy[0], first)
        );
        assert!(
            dist(*r.xy.last().unwrap(), last) < 60.0,
            "ends at the last stop"
        );
    }

    #[test]
    fn consolidated_line_lands_on_the_true_corridor() {
        let truth = corridor();
        let stops = stops_along(&truth, "S");
        let mut rng = Rng::new(20260921);
        let tracks = fleet(&truth, &mut rng);
        let p = Params::default();
        let built = build_line(&tracks, &stops, &p).expect("a line");
        assert_eq!(built.counts.runs_used, 9);
        assert_eq!(built.buses, 5);
        assert!(!built.consolidation.degraded);
        let pl = Planar::new(ORIGIN.0, ORIGIN.1);
        let line: Vec<(f64, f64)> = built.line.iter().map(|&q| pl.xy(q)).collect();

        // the line sits on the true road (measured every 10 m along it, not
        // at its vertices, which crowd into the corners)...
        let to_truth = distances(&resample_xy(&line, 10.0), &truth);
        let median = to_truth[to_truth.len() / 2];
        let p90 = to_truth[to_truth.len() * 9 / 10];
        eprintln!(
            "self-test: {} runs, {} points, median {median:.1} m, p90 {p90:.1} m, max {:.1} m, {:?}",
            built.counts.runs_used,
            line.len(),
            to_truth.last().unwrap(),
            built.consolidation
        );
        assert!(median < 4.0, "median {median:.1} m");
        assert!(p90 < 10.0, "p90 {p90:.1} m");
        // ...and the true road is all there
        let truth_pts = resample_xy(&truth, 20.0);
        let from_truth = distances(&truth_pts, &line);
        let covered =
            from_truth.iter().filter(|&&d| d <= 15.0).count() as f64 / from_truth.len() as f64;
        assert!(covered >= 0.95, "{covered:.3} of the corridor within 15 m");
        // in the direction of the route, first stop to last
        let (s0, sn) = (
            pl.xy((stops[0].lat, stops[0].lon)),
            pl.xy((stops.last().unwrap().lat, stops.last().unwrap().lon)),
        );
        assert!(dist(line[0], s0) < 60.0, "starts at the first stop");
        assert!(
            dist(*line.last().unwrap(), sn) < 60.0,
            "ends at the last stop"
        );
        // every stop is on it
        let stops_xy: Vec<(f64, f64)> = stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
        assert_eq!(stop_coverage(&line, &stops_xy, 30.0), 1.0);
        // and the length is the road's, not a zig-zag's
        let len: f64 = line.windows(2).map(|w| dist(w[0], w[1])).sum();
        let truth_len: f64 = truth.windows(2).map(|w| dist(w[0], w[1])).sum();
        assert!(
            (len - truth_len).abs() / truth_len < 0.03,
            "{len:.0} m vs {truth_len:.0} m"
        );
        // simplified: nothing like one point per ping
        assert!(line.len() < 150, "{} points", line.len());

        // deterministic
        let mut rng2 = Rng::new(20260921);
        let again = build_line(&fleet(&truth, &mut rng2), &stops, &p).unwrap();
        assert_eq!(
            osrm::encode_polyline(&built.line),
            osrm::encode_polyline(&again.line)
        );
        eprintln!(
            "self-test: {} runs, {} points, median {median:.1} m, p90 {p90:.1} m, corridor covered {:.1}%, length {len:.0}/{truth_len:.0} m",
            built.counts.runs_used,
            line.len(),
            covered * 100.0
        );
    }

    #[test]
    fn a_route_that_ends_where_it_began_does_not_fold() {
        // out along a road, round a block, and back along the other
        // carriageway 20 m away: every cell near the terminus is visited at
        // the start and again at the end
        let loop_path = vec![
            (0.0, 0.0),
            (2_000.0, 0.0),
            (2_000.0, 400.0),
            (2_400.0, 400.0),
            (2_400.0, -60.0),
            (2_100.0, -60.0),
            (2_100.0, -20.0),
            (0.0, -20.0),
        ];
        let stops = stops_along(&loop_path, "L");
        let mut rng = Rng::new(11);
        let tracks: Vec<Track> = (0..5)
            .map(|i| {
                bus_day(
                    &format!("loop-{i}"),
                    &[loop_path.clone()],
                    &mut rng,
                    1_758_000_000 + i * 600,
                )
            })
            .collect();
        let built = build_line(&tracks, &stops, &Params::default()).expect("a line");
        assert_eq!(built.counts.runs_used, 5);
        let pl = Planar::new(ORIGIN.0, ORIGIN.1);
        let line: Vec<(f64, f64)> = built.line.iter().map(|&q| pl.xy(q)).collect();
        let len: f64 = line.windows(2).map(|w| dist(w[0], w[1])).sum();
        let truth_len: f64 = loop_path.windows(2).map(|w| dist(w[0], w[1])).sum();
        // a folded line would be about half as long
        assert!(
            (len - truth_len).abs() / truth_len < 0.06,
            "{len:.0} m vs {truth_len:.0} m"
        );
        // out on the north carriageway, back on the south one, in that order
        assert!(
            dist(line[0], (0.0, 0.0)) < 40.0,
            "starts at the terminus: {:?}",
            line[0]
        );
        assert!(
            dist(*line.last().unwrap(), (0.0, -20.0)) < 40.0,
            "ends there too: {:?}",
            line.last()
        );
        let at_1000: Vec<usize> = (0..line.len() - 1)
            .filter(|&i| (line[i].0 - 1_000.0) * (line[i + 1].0 - 1_000.0) <= 0.0)
            .collect();
        assert_eq!(at_1000.len(), 2, "crosses x = 1000 m once each way");
        let y_at = |i: usize| {
            let (a, b) = (line[i], line[i + 1]);
            a.1 + (b.1 - a.1) * (1_000.0 - a.0) / (b.0 - a.0)
        };
        assert!(y_at(at_1000[0]).abs() < 8.0 && (y_at(at_1000[1]) + 20.0).abs() < 8.0);
        let (y_out, y_back) = (y_at(at_1000[0]), y_at(at_1000[1]));
        assert!(y_out > y_back, "outbound {y_out:.1}, back {y_back:.1}");
        // the stops: all but those on the block's corners, which 20-second
        // chords cut (OSRM puts the corners back)
        let stops_xy: Vec<(f64, f64)> = stops.iter().map(|s| pl.xy((s.lat, s.lon))).collect();
        assert!(stop_coverage(&line, &stops_xy, 30.0) >= 0.8);
    }

    #[test]
    fn too_few_runs_is_not_enough() {
        let truth = corridor();
        let stops = stops_along(&truth, "S");
        let mut rng = Rng::new(5);
        let tracks = vec![bus_day(
            "solo",
            &[truth.clone(), reversed(&truth)],
            &mut rng,
            1_758_000_000,
        )];
        let counts = build_line(&tracks, &stops, &Params::default()).unwrap_err();
        assert_eq!(counts.runs_used, 1);
        assert!(counts.runs_seen >= 2);
    }

    #[test]
    fn a_feed_gap_and_a_terminal_dwell_split_runs() {
        let pl = Planar::new(ORIGIN.0, ORIGIN.1);
        let p = Params::default();
        let mut rng = Rng::new(9);
        let leg: Vec<(f64, f64)> = vec![(0.0, 0.0), (3_000.0, 0.0)];
        let mut pings = drive(&leg, &mut rng, 0, 0.0);
        let t = pings.last().unwrap().t;
        pings.extend(stand((3_000.0, 0.0), &mut rng, t + 20, 600));
        let t = pings.last().unwrap().t;
        pings.extend(drive(&reversed(&leg), &mut rng, t + 20, 0.0));
        let t = pings.last().unwrap().t;
        // a 20 minute hole in the feed, then out again
        pings.extend(drive(&leg, &mut rng, t + 1_200, 0.0));
        let runs = split_runs(&clean(&pings, p.max_speed_kmh), &pl, &p);
        assert_eq!(runs.len(), 3);
    }

    #[test]
    fn queries_are_bounded_and_pass_the_guard() {
        let stops = vec![
            Stop {
                stop_id: "a".into(),
                lat: 13.0,
                lon: 80.2,
            },
            Stop {
                stop_id: "b".into(),
                lat: 13.05,
                lon: 80.25,
            },
        ];
        let bbox = Bbox::around(&stops, 1_000.0).unwrap();
        assert!(bbox.min_lat < 13.0 - 0.008 && bbox.max_lon > 80.25 + 0.008);
        let q1 = bus_days_sql(
            DEFAULT_TABLE,
            1_757_000_000,
            1_758_000_000,
            "21g",
            &bbox,
            120,
            4,
        );
        let q2 = tracks_sql(
            DEFAULT_TABLE,
            1_757_000_000,
            1_757_086_400,
            &["864'1".into(), "x".into()],
            "21g",
            &bbox,
            20,
        );
        for q in [&q1, &q2] {
            let paged = format!("{q} LIMIT 100 OFFSET 0");
            crate::services::clickhouse_reader::assert_read_only(&paged).unwrap();
            assert!(
                q.contains("timestamp >= toDateTime(1757") && q.contains("timestamp <= now()"),
                "time bound: {q}"
            );
            assert!(
                q.contains("lat BETWEEN") && q.contains("long BETWEEN"),
                "box: {q}"
            );
        }
        assert!(
            q1.contains("= '21g'") && q1.contains("LIMIT 4 BY day"),
            "{q1}"
        );
        assert!(
            q2.contains("IN ('864\\'1', 'x')") && q2.contains("IN ('21g', '')"),
            "{q2}"
        );
        assert_eq!(route_label(" 21G "), Some("21g".into()));
        assert_eq!(route_label("  "), None);
        assert_eq!(route_label("a;b"), None);
        assert_eq!(
            parse_packed("1757000000,13.1,80.2|1757000020,13.2,80.3|junk"),
            vec![
                Ping {
                    t: 1_757_000_000,
                    lat: 13.1,
                    lon: 80.2
                },
                Ping {
                    t: 1_757_000_020,
                    lat: 13.2,
                    lon: 80.3
                },
            ]
        );
    }

    #[test]
    fn bus_days_are_spread_over_the_window() {
        let rows: Vec<(String, String, u64)> = [
            ("a", "2026-09-20", 900),
            ("b", "2026-09-20", 800),
            ("c", "2026-09-20", 700),
            ("a", "2026-09-19", 500),
            ("d", "2026-09-18", 400),
            ("e", "2026-09-18", 450),
        ]
        .iter()
        .map(|(d, day, n)| (d.to_string(), day.to_string(), *n))
        .collect();
        let chosen = spread_bus_days(&rows, 4);
        assert_eq!(
            chosen,
            vec![
                ("a".to_string(), "2026-09-20".to_string()),
                ("a".to_string(), "2026-09-19".to_string()),
                ("e".to_string(), "2026-09-18".to_string()),
                ("b".to_string(), "2026-09-20".to_string()),
            ]
        );
        assert_eq!(spread_bus_days(&rows, 50).len(), 6);
    }

    #[test]
    fn service_days_are_indian_days() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 9, 21).unwrap();
        // 2026-09-21 00:00 IST is 2026-09-20 18:30 UTC
        assert_eq!(
            day_start(d),
            chrono::DateTime::parse_from_rfc3339("2026-09-20T18:30:00Z")
                .unwrap()
                .timestamp()
        );
        assert_eq!(today(day_start(d)), d);
        assert_eq!(today(day_start(d) - 1), d.pred_opt().unwrap());
    }
}
