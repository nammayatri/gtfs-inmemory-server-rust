//! Every route of a feed from one read of the week (docs/gtfs-editor.md section
//! 19), where [`GpsLine::gps_path`] reads per route.
//!
//! Pings are taken by the GTFS `route_id` the trip was assigned to (filled from
//! the trip assignment, as the control center shows them), not by the route
//! number the crew typed: that label is often another variant's (104A trips of
//! 1987 typed `104ACT`) and covers every variant of a number at once.
//!
//!  1. **Index**: one query per day for the whole fleet: every (route id, bus)
//!     with its ping count and first and last ping of that route. The answer is
//!     packed one row per bucket of route ids, so it stays a few dozen rows.
//!  2. **Choose**: per route id, its busiest bus-days spread over the week.
//!  3. **Groups**: the route ids in `groups` groups. Per group, the chosen buses'
//!     pings over the hours they ran a route, in answers of at most `page_rows`
//!     bus-hours, averaged to `bucket_s`, each point with the route id it
//!     carried. They are held only while the group's routes are built.
//!
//! A per-route read scans every day once per route; this scans each hour a few
//! times per group, whatever the number of routes.

use super::*;
use std::collections::HashSet;
use std::sync::Arc;

/// Index answers: route numbers hashed into this many rows per day.
pub const INDEX_BUCKETS: u64 = 32;
/// A query that fails is asked this many times in all.
const ATTEMPTS: usize = 2;

/// One bus running one route on one day.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDay {
    pub device: String,
    pub date: chrono::NaiveDate,
    pub pings: u64,
    pub first: i64,
    pub last: i64,
}

/// What the index read: route id to its bus-days, every day of the week.
#[derive(Debug, Default)]
pub struct WeekIndex {
    pub from: Option<chrono::NaiveDate>,
    pub to: Option<chrono::NaiveDate>,
    pub days: u32,
    pub days_read: u32,
    pub days_unread: u32,
    pub by_route: HashMap<String, Vec<RouteDay>>,
    pub queries: u64,
}

impl WeekIndex {
    pub fn complete(&self) -> bool {
        self.days_unread == 0
    }
}

/// A group's pings, sorted out per route id.
#[derive(Debug, Default)]
pub struct GroupRead {
    /// Per route id.
    pub tracks: HashMap<String, Arc<Vec<Track>>>,
    /// Per route id: (bus-days chosen, bus-days read, pings, points).
    pub counts: HashMap<String, (usize, usize, u64, usize)>,
    /// Every window answered.
    pub complete: bool,
    /// Route ids with a bus-day in a window that did not answer.
    pub incomplete: HashSet<String>,
    pub queries: u64,
    pub read_seconds: f64,
}

/// A route id as the index keys it: trimmed; None when it cannot be one.
pub fn route_key(v: &str) -> Option<String> {
    let t = v.trim();
    (!t.is_empty() && t.len() <= 64 && !t.contains(';') && !t.contains('\n')).then(|| t.to_string())
}

/// The group a route id is read in: the same on every pod and every run.
pub fn group_of(key: &str, groups: usize) -> usize {
    // FNV-1a
    let mut h: u64 = 0xcbf29ce484222325;
    for b in key.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    (h % groups.max(1) as u64) as usize
}

/// Step 1 for one day `[from, to)`: every (route id, bus) with its pings and
/// first and last ping of that route, packed per bucket of route ids as
/// `hex(route_id),hex(device),n,first,last|...`.
pub fn index_sql(table: &str, from: i64, to: i64, buckets: u64) -> String {
    format!(
        "SELECT cityHash64(num) % {buckets} AS bucket, count() AS pairs, \
         arrayStringConcat(groupArray(concat(hex(num), ',', hex(device), ',', toString(n), ',', \
         toString(first_seen), ',', toString(last_seen))), '|') AS packed \
         FROM (\
         SELECT toString({COL_ROUTE_ID}) AS num, toString({COL_DEVICE}) AS device, count() AS n, \
         toUnixTimestamp(min({COL_TIME})) AS first_seen, toUnixTimestamp(max({COL_TIME})) AS last_seen \
         FROM {table} \
         WHERE {COL_TIME} >= toDateTime({from}) AND {COL_TIME} < toDateTime({to}) AND {COL_TIME} <= now() \
         AND {COL_ROUTE_ID} != '' AND {COL_DEVICE} != '' \
         GROUP BY num, device) \
         GROUP BY bucket \
         ORDER BY bucket \
         LIMIT {buckets}"
    )
}

fn unhex(v: &str) -> Option<String> {
    String::from_utf8(hex::decode(v.trim()).ok()?).ok()
}

/// An [`index_sql`] answer as (route id, bus-day); a route id written with
/// stray spaces is merged into the same one.
pub fn parse_index(rows: &[Vec<String>], date: chrono::NaiveDate) -> Vec<(String, RouteDay)> {
    let mut merged: BTreeMap<(String, String), RouteDay> = BTreeMap::new();
    for packed in rows.iter().filter_map(|r| r.get(2)) {
        for entry in packed.split('|') {
            let f: Vec<&str> = entry.split(',').collect();
            if f.len() != 5 {
                continue;
            }
            let (Some(label), Some(device), Ok(n), Ok(first), Ok(last)) = (
                unhex(f[0]),
                unhex(f[1]),
                f[2].parse::<u64>(),
                f[3].parse::<i64>(),
                f[4].parse::<i64>(),
            ) else {
                continue;
            };
            let Some(key) = route_key(&label) else {
                continue;
            };
            if device.is_empty() {
                continue;
            }
            merged
                .entry((key, device.clone()))
                .and_modify(|d| {
                    d.pings += n;
                    d.first = d.first.min(first);
                    d.last = d.last.max(last);
                })
                .or_insert(RouteDay {
                    device,
                    date,
                    pings: n,
                    first,
                    last,
                });
        }
    }
    merged.into_iter().map(|((k, _), d)| (k, d)).collect()
}

/// Per route id, the bus-days to read: at least `min_pings` pings, the
/// busiest of each day first and the newest day first, `want` in all, so a
/// line draws on the whole week (as [`GpsLine::gps_path`] chooses).
pub fn choose(index: &WeekIndex, want: usize, min_pings: u64) -> HashMap<String, Vec<RouteDay>> {
    let mut out = HashMap::new();
    for (route, days) in &index.by_route {
        let mut by_date: BTreeMap<chrono::NaiveDate, Vec<&RouteDay>> = BTreeMap::new();
        for d in days.iter().filter(|d| d.pings >= min_pings) {
            by_date.entry(d.date).or_default().push(d);
        }
        for buses in by_date.values_mut() {
            buses.sort_by(|a, b| b.pings.cmp(&a.pings).then(a.device.cmp(&b.device)));
        }
        let dates: Vec<&Vec<&RouteDay>> = by_date.values().rev().collect();
        let mut chosen = Vec::new();
        let most = dates.iter().map(|b| b.len()).max().unwrap_or(0);
        'pick: for rank in 0..most {
            for buses in &dates {
                if chosen.len() >= want {
                    break 'pick;
                }
                if let Some(d) = buses.get(rank) {
                    chosen.push((*d).clone());
                }
            }
        }
        if !chosen.is_empty() {
            out.insert(route.clone(), chosen);
        }
    }
    out
}

/// Step 3 for some buses over `[from, to)`: their pings averaged to one point
/// per `bucket_s`, each with a route id it carried in that bucket (empty when
/// none), packed one bus-hour per row as `t,lat,lon,hex(route_id)|...`.
pub fn group_tracks_sql(
    table: &str,
    from: i64,
    to: i64,
    devices: &[String],
    bucket_s: i64,
    limit: usize,
) -> String {
    format!(
        "SELECT device, intDiv(t, 3600) AS h, count() AS points, sum(n) AS pings, \
         arrayStringConcat(groupArray(concat(toString(t), ',', toString(la), ',', toString(lo), ',', hex(lab))), '|') AS track \
         FROM (\
         SELECT toString({COL_DEVICE}) AS device, intDiv(toUnixTimestamp({COL_TIME}), {bucket_s}) * {bucket_s} AS t, \
         round(avg({COL_LAT}), 6) AS la, round(avg({COL_LON}), 6) AS lo, count() AS n, \
         anyIf(ifNull(toString({COL_ROUTE_ID}), ''), ifNull(toString({COL_ROUTE_ID}), '') != '') AS lab \
         FROM {table} \
         WHERE {COL_TIME} >= toDateTime({from}) AND {COL_TIME} < toDateTime({to}) AND {COL_TIME} <= now() \
         AND {COL_DEVICE} IN ({list}) \
         GROUP BY device, t) \
         GROUP BY device, h \
         ORDER BY device, h \
         LIMIT {limit}",
        list = in_list(devices),
    )
}

/// One packed row of [`group_tracks_sql`]: (ping, route id, empty when none).
pub fn parse_labelled(track: &str) -> Vec<(Ping, String)> {
    track
        .split('|')
        .filter_map(|p| {
            let mut it = p.split(',');
            let t = it.next()?.trim().parse::<i64>().ok()?;
            let lat = it.next()?.trim().parse::<f64>().ok()?;
            let lon = it.next()?.trim().parse::<f64>().ok()?;
            let label = unhex(it.next().unwrap_or(""))
                .and_then(|l| route_key(&l))
                .unwrap_or_default();
            Some((Ping { t, lat, lon }, label))
        })
        .collect()
}

/// A bus-day's pings out of its bus's day: inside its span (with a margin for a
/// trip whose first pings came before the assignment) and carrying its route
/// id or none.
pub fn attribute(points: &[(Ping, String)], route: &str, d: &RouteDay) -> Vec<Ping> {
    let (a, b) = (d.first - SPAN_MARGIN_S, d.last + SPAN_MARGIN_S);
    points
        .iter()
        .filter(|(p, l)| p.t >= a && p.t <= b && (l.is_empty() || l == route))
        .map(|(p, _)| *p)
        .collect()
}

impl GpsLine {
    /// Step 1: the index of the `days` days up to `now`, a few days at a time.
    /// A day whose query fails twice is left unread.
    pub async fn read_week_index(&self, now: i64) -> WeekIndex {
        let s = &self.settings;
        let before = self.reader.sent();
        let last_day = today(now);
        let dates: Vec<chrono::NaiveDate> = (0..s.days)
            .map(|k| last_day - chrono::Duration::days(i64::from(k)))
            .collect();
        let answers: Vec<(chrono::NaiveDate, Option<Vec<Vec<String>>>)> =
            futures::stream::iter(dates)
                .map(|date| async move {
                    let (a, b) = (day_start(date), (day_start(date) + 86_400).min(now));
                    let sql = index_sql(&s.table, a, b, INDEX_BUCKETS);
                    (date, self.rows_retrying(&sql).await)
                })
                .buffered(s.clickhouse.max_concurrent.max(1))
                .collect()
                .await;
        let mut index = WeekIndex {
            days: s.days,
            to: Some(last_day),
            ..WeekIndex::default()
        };
        for (date, rows) in answers {
            let Some(rows) = rows else {
                index.days_unread += 1;
                continue;
            };
            index.days_read += 1;
            index.from = Some(index.from.map_or(date, |f| f.min(date)));
            for (key, d) in parse_index(&rows, date) {
                index.by_route.entry(key).or_default().push(d);
            }
        }
        index.queries = self.reader.sent() - before;
        index
    }

    /// Step 3 for one group of route ids: the chosen bus-days' pings, per
    /// day in stretches of whole hours of at most `page_rows` bus-hours each.
    pub async fn read_group(
        &self,
        chosen: &HashMap<String, Vec<RouteDay>>,
        routes: &[String],
        now: i64,
    ) -> GroupRead {
        let s = &self.settings;
        let started = Instant::now();
        let before = self.reader.sent();
        // per day: each bus's span over every route of the group it ran
        let mut spans: BTreeMap<chrono::NaiveDate, BTreeMap<String, (i64, i64)>> = BTreeMap::new();
        for n in routes {
            for d in chosen.get(n).into_iter().flatten() {
                let e = spans
                    .entry(d.date)
                    .or_default()
                    .entry(d.device.clone())
                    .or_insert((i64::MAX, i64::MIN));
                e.0 = e.0.min(d.first);
                e.1 = e.1.max(d.last);
            }
        }
        let mut windows: Vec<(i64, i64, Vec<String>, usize)> = Vec::new();
        for (date, buses) in &spans {
            let (day_a, day_b) = (day_start(*date), (day_start(*date) + 86_400).min(now));
            // buses that start alike share windows, so fewer empty hours are asked for
            let mut order: Vec<(&String, &(i64, i64))> = buses.iter().collect();
            order.sort_by_key(|(dev, span)| (span.0, (*dev).clone()));
            for chunk in order.chunks(s.page_rows.max(1)) {
                let a = chunk.iter().map(|(_, sp)| sp.0).min().unwrap_or(day_a);
                let b = chunk.iter().map(|(_, sp)| sp.1).max().unwrap_or(day_b);
                let a = (a - SPAN_MARGIN_S).max(day_a);
                let b = (b + SPAN_MARGIN_S + 1).min(day_b);
                let devices: Vec<String> = chunk.iter().map(|(d, _)| (*d).clone()).collect();
                windows.extend(track_windows(a, b, &devices, s.page_rows));
            }
        }
        let answers: Vec<Option<Vec<Vec<String>>>> = futures::stream::iter(&windows)
            .map(|(a, b, devices, limit)| async move {
                let sql = group_tracks_sql(&s.table, *a, *b, devices, s.params.bucket_s, *limit);
                self.rows_retrying(&sql).await
            })
            .buffered(s.clickhouse.max_concurrent.max(1))
            .collect()
            .await;
        let failed: Vec<&(i64, i64, Vec<String>, usize)> = windows
            .iter()
            .zip(&answers)
            .filter(|(_, a)| a.is_none())
            .map(|(w, _)| w)
            .collect();
        let complete = failed.is_empty();
        // a window that did not answer leaves only the routes whose buses it held unread
        let mut incomplete = HashSet::new();
        for n in routes {
            let hit = chosen.get(n).into_iter().flatten().any(|d| {
                failed.iter().any(|(a, b, devices, _)| {
                    devices.contains(&d.device)
                        && d.first - SPAN_MARGIN_S < *b
                        && d.last + SPAN_MARGIN_S >= *a
                })
            });
            if hit {
                incomplete.insert(n.clone());
            }
        }
        let mut by_device: HashMap<String, Vec<(Ping, String)>> = HashMap::new();
        for r in answers.into_iter().flatten().flatten() {
            let (Some(device), Some(track)) = (r.first(), r.get(4)) else {
                continue;
            };
            by_device
                .entry(device.clone())
                .or_default()
                .extend(parse_labelled(track));
        }
        for points in by_device.values_mut() {
            points.sort_by_key(|(p, _)| p.t);
        }
        let mut out = GroupRead {
            complete,
            incomplete,
            ..GroupRead::default()
        };
        for n in routes {
            let days = chosen.get(n).map(Vec::as_slice).unwrap_or(&[]);
            let mut tracks = Vec::new();
            let (mut points, mut pings, mut read) = (0usize, 0u64, 0usize);
            for d in days {
                let Some(all) = by_device.get(&d.device) else {
                    continue;
                };
                let mine = attribute(all, n, d);
                if mine.is_empty() {
                    continue;
                }
                read += 1;
                points += mine.len();
                pings += d.pings;
                tracks.push(Track {
                    device: d.device.clone(),
                    pings: mine,
                });
            }
            out.counts
                .insert(n.clone(), (days.len(), read, pings, points));
            out.tracks.insert(n.clone(), Arc::new(tracks));
        }
        out.queries = self.reader.sent() - before;
        out.read_seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        out
    }

    async fn rows_retrying(&self, sql: &str) -> Option<Vec<Vec<String>>> {
        for attempt in 1..=ATTEMPTS {
            match self
                .reader
                .rows(sql, self.settings.clickhouse.query_timeout)
                .await
            {
                Ok(rows) => return Some(rows),
                Err(e) if attempt == ATTEMPTS => {
                    tracing::warn!("gps batch: query failed: {e}");
                }
                Err(_) => {}
            }
        }
        None
    }
}

/// Steps 3 and 4 of one route from its number's tracks, with the evidence the
/// sync's gate reads; `Err` with the counts when too few runs passed its stops.
pub fn build(
    tracks: &[Track],
    stops: &[Stop],
    p: &Params,
    mut evidence: Value,
) -> Result<(Vec<(f64, f64)>, Value), Value> {
    match build_line(tracks, stops, p) {
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
            Ok((built.line, evidence))
        }
        Err(counts) => {
            evidence["runs_seen"] = json!(counts.runs_seen);
            evidence["runs_used"] = json!(counts.runs_used);
            evidence["min_runs"] = json!(p.min_runs);
            Err(evidence)
        }
    }
}

/// The evidence a route starts with: what was read for it.
pub fn evidence_for(index: &WeekIndex, group: &GroupRead, route: &str, stops: usize) -> Value {
    let (chosen, read, pings, points) = group.counts.get(route).copied().unwrap_or_default();
    let complete = index.complete() && !group.incomplete.contains(route);
    json!({
        "from": index.from.map(|d| d.to_string()),
        "to": index.to.map(|d| d.to_string()),
        "days": index.days,
        "days_read": index.days_read,
        "days_unread": index.days_unread,
        "stopped": if complete { "complete" } else { "budget" },
        "route_id": route,
        "stops": stops,
        "bus_days": chosen,
        "bus_days_read": read,
        "pings": pings,
        "points_read": points,
        "truncated": false,
        "queries": index.queries + group.queries,
        "read_seconds": group.read_seconds,
        "read": "batch",
    })
}

#[cfg(test)]
mod tests {
    use super::super::tests::{corridor, fleet, settings_for_tests, stops_along, Rng};
    use super::*;
    use crate::services::clickhouse_reader::assert_read_only;
    use std::sync::Arc;

    #[test]
    fn its_statements_pass_the_read_only_guard() {
        assert_read_only(&index_sql(DEFAULT_TABLE, 0, 86_400, INDEX_BUCKETS)).unwrap();
        let devices = vec!["dev-a".to_string(), "dev-b".to_string()];
        assert_read_only(&group_tracks_sql(DEFAULT_TABLE, 0, 3_600, &devices, 20, 2)).unwrap();
    }

    #[test]
    fn a_route_id_with_stray_spaces_is_one_bus_day() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let e = |label: &str, dev: &str, n: u64, a: i64, b: i64| {
            format!("{},{},{n},{a},{b}", hex::encode(label), hex::encode(dev))
        };
        let rows = vec![vec![
            "0".into(),
            "3".into(),
            [
                e("1987", "d1", 100, 1_000, 2_000),
                e(" 1987", "d1", 30, 900, 2_500),
                e("1983", "d1", 50, 3_000, 4_000),
            ]
            .join("|"),
        ]];
        let mut got = parse_index(&rows, date);
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "1983");
        assert_eq!(got[1].0, "1987");
        assert_eq!(
            (got[1].1.pings, got[1].1.first, got[1].1.last),
            (130, 900, 2_500)
        );
    }

    #[test]
    fn the_busiest_bus_of_each_day_comes_first() {
        let day = |k: i64| {
            chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap() - chrono::Duration::days(k)
        };
        let bd = |dev: &str, k: i64, n: u64| RouteDay {
            device: dev.into(),
            date: day(k),
            pings: n,
            first: 0,
            last: 1,
        };
        let mut index = WeekIndex::default();
        index.by_route.insert(
            "1987".into(),
            vec![
                bd("a", 0, 500),
                bd("b", 0, 900),
                bd("c", 1, 700),
                bd("d", 1, 50),
                bd("e", 2, 300),
            ],
        );
        let chosen = choose(&index, 3, 120);
        let got: Vec<&str> = chosen["1987"].iter().map(|d| d.device.as_str()).collect();
        assert_eq!(got, vec!["b", "c", "e"]);
        let all = choose(&index, 10, 120);
        assert_eq!(all["1987"].len(), 4, "the bus with 50 pings is too thin");
    }

    #[test]
    fn a_failed_window_leaves_only_its_routes_unread() {
        let index = WeekIndex {
            days: 7,
            days_read: 7,
            ..WeekIndex::default()
        };
        let group = GroupRead {
            complete: false,
            incomplete: HashSet::from(["1987".to_string()]),
            ..GroupRead::default()
        };
        assert_eq!(
            evidence_for(&index, &group, "1987", 30)["stopped"],
            "budget"
        );
        assert_eq!(
            evidence_for(&index, &group, "1983", 30)["stopped"],
            "complete"
        );
    }

    #[test]
    fn a_route_is_always_in_the_same_group() {
        assert_eq!(group_of("1987", 8), group_of("1987", 8));
        let groups: BTreeSet<usize> = (0..200).map(|i| group_of(&format!("R{i}"), 8)).collect();
        assert_eq!(groups.len(), 8);
    }

    #[test]
    fn a_bus_days_pings_carry_its_route_or_none() {
        let p = |t: i64| Ping {
            t,
            lat: 13.0,
            lon: 80.2,
        };
        let points = vec![
            (p(100), "1987".to_string()),
            (p(200), String::new()),
            (p(300), "1983".to_string()),
            (p(100_000), "1987".to_string()),
        ];
        let d = RouteDay {
            device: "a".into(),
            date: chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
            pings: 3,
            first: 100,
            last: 300,
        };
        let got: Vec<i64> = attribute(&points, "1987", &d).iter().map(|p| p.t).collect();
        assert_eq!(got, vec![100, 200]);
    }

    /// The fleet of `gps_line`'s self-test on each of the last `days` days, on
    /// route id R1, answering the batch's two statements.
    struct FakeFleet {
        days: BTreeMap<i64, Vec<Track>>,
        sent: std::sync::atomic::AtomicU64,
    }

    fn numbers(sql: &str, pattern: &str) -> Vec<i64> {
        regex::Regex::new(pattern)
            .unwrap()
            .captures_iter(sql)
            .map(|c| c[1].parse().unwrap())
            .collect()
    }

    #[async_trait::async_trait]
    impl RowSource for FakeFleet {
        async fn rows(
            &self,
            sql: &str,
            _limit: Duration,
        ) -> Result<Vec<Vec<String>>, ClickHouseError> {
            self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_read_only(sql).unwrap();
            let t = numbers(sql, r"toDateTime\((\d+)\)");
            let (a, b) = (t[0], t[1]);
            let tracks = self.days.values().flatten();
            if sql.contains("cityHash64") {
                let entries: Vec<String> = tracks
                    .filter_map(|tr| {
                        let seen: Vec<&Ping> =
                            tr.pings.iter().filter(|p| p.t >= a && p.t < b).collect();
                        (!seen.is_empty()).then(|| {
                            format!(
                                "{},{},{},{},{}",
                                hex::encode("R1"),
                                hex::encode(&tr.device),
                                seen.len(),
                                seen[0].t,
                                seen[seen.len() - 1].t
                            )
                        })
                    })
                    .collect();
                return Ok(if entries.is_empty() {
                    vec![]
                } else {
                    vec![vec![
                        "0".into(),
                        entries.len().to_string(),
                        entries.join("|"),
                    ]]
                });
            }
            let list = regex::Regex::new(r"deviceId IN \(([^)]*)\)")
                .unwrap()
                .captures(sql)
                .map(|c| c[1].to_string())
                .unwrap_or_default();
            let wanted: Vec<String> = list
                .split(", ")
                .map(|d| d.trim_matches('\'').to_string())
                .collect();
            let mut out = vec![];
            for tr in tracks.filter(|tr| wanted.contains(&tr.device)) {
                let mut hours: BTreeMap<i64, Vec<String>> = BTreeMap::new();
                for p in tr.pings.iter().filter(|p| p.t >= a && p.t < b) {
                    hours.entry(p.t.div_euclid(3600)).or_default().push(format!(
                        "{},{:.6},{:.6},{}",
                        p.t,
                        p.lat,
                        p.lon,
                        hex::encode("R1")
                    ));
                }
                for (h, pts) in hours {
                    out.push(vec![
                        tr.device.clone(),
                        h.to_string(),
                        pts.len().to_string(),
                        pts.len().to_string(),
                        pts.join("|"),
                    ]);
                }
            }
            Ok(out)
        }

        fn sent(&self) -> u64 {
            self.sent.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn fake_fleet(now: i64, days: i64) -> FakeFleet {
        let truth = corridor();
        let mut out = BTreeMap::new();
        for k in 1..=days {
            let start = day_start(today(now) - chrono::Duration::days(k));
            let mut rng = Rng::new(100 + k as u64);
            let tracks: Vec<Track> = fleet(&truth, &mut rng)
                .into_iter()
                .map(|mut tr| {
                    tr.device = format!("{}-{k}", tr.device);
                    for p in &mut tr.pings {
                        p.t = p.t - 1_758_000_000 + start + 6 * 3600;
                    }
                    tr
                })
                .collect();
            out.insert(start, tracks);
        }
        FakeFleet {
            days: out,
            sent: Default::default(),
        }
    }

    #[tokio::test]
    async fn one_read_of_the_week_builds_the_route() {
        let now = chrono::Utc::now().timestamp();
        let rows = Arc::new(fake_fleet(now, 6));
        let mut settings = settings_for_tests();
        settings.days = 7;
        let gps = GpsLine::with_source(settings, rows.clone()).unwrap();

        let index = gps.read_week_index(now).await;
        assert_eq!((index.days_read, index.days_unread), (7, 0));
        assert_eq!(index.queries, 7, "one index query a day");
        let chosen = choose(&index, 12, 120);
        let routes = vec!["R1".to_string()];
        let group = gps.read_group(&chosen, &routes, now).await;
        assert!(group.complete);
        assert_eq!(group.tracks["R1"].len(), 12);

        let stops = stops_along(&corridor(), "S");
        let base = evidence_for(&index, &group, "R1", stops.len());
        let (line, ev) = build(&group.tracks["R1"], &stops, &Params::default(), base).unwrap();
        assert!(line.len() > 10, "{ev}");
        assert_eq!(ev["stopped"], "complete");
        assert!(ev["runs_used"].as_u64().unwrap() >= 5, "{ev}");
    }
}
