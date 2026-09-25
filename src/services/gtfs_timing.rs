//! A trip's stop times from the editor's tables (docs/gtfs-editor.md section 16).
//!
//! Nothing per stop per trip is stored. A trip is a pattern, a timing profile
//! (or the pattern's default timing) and a reference time, and its stop times
//! are the reference time plus the profile's offsets (16.3). This module is that
//! arithmetic, pure and shared by the GIMS loader, which serves the times, and
//! the editor, which validates and carries profiles over when a stop list
//! changes:
//!
//! - [`default_offsets`]: the generator's formula, `i·(run + dwell)`, the last
//!   stop departing when it arrives - chennai_bus's timetable to the second.
//! - [`pattern_id`]: the public id `gtfs_preprocessor.py` gives a stop order.
//! - [`carry_over`]: a profile's offsets on a pattern whose stops changed.
//! - GTFS clock times, which run past 24:00.

use md5::{Digest, Md5};

/// A trip's stop times may run to 47:59:59 (a service day and its night).
pub const MAX_TIME_S: i32 = 48 * 3600 - 1;

/// A hop faster than this, in a straight line, is a timing nobody drives.
pub const IMPLAUSIBLE_FAST_KMH: f64 = 100.0;
/// A hop slower than this is a timing that forgot a stop, or a typo.
pub const IMPLAUSIBLE_SLOW_KMH: f64 = 2.0;

/// Speed assumed when a pattern gives nothing to measure one from (every hop of
/// a carried-over profile is new, or takes no time).
const FALLBACK_SPEED_MPS: f64 = 20.0 / 3.6;

/// Arrival and departure offsets of a pattern's served stops, in seconds from a
/// trip's reference time, in stop order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offsets {
    pub arrival: Vec<i32>,
    pub departure: Vec<i32>,
}

impl Offsets {
    pub fn len(&self) -> usize {
        self.arrival.len()
    }

    pub fn is_empty(&self) -> bool {
        self.arrival.is_empty()
    }
}

/// The default timing of a pattern of `stops` served stops (16.1): stop `i`
/// arrives at `i·(run + dwell)` and departs `dwell` later, the last stop
/// departing when it arrives. With the feed defaults (120, 15) this is
/// `generate_trips_from_db.py`'s `start + 135·i` to the second.
pub fn default_offsets(stops: usize, run_s: i32, dwell_s: i32) -> Offsets {
    let last = stops.saturating_sub(1);
    let arrival: Vec<i32> = (0..stops).map(|i| i as i32 * (run_s + dwell_s)).collect();
    let departure = arrival
        .iter()
        .enumerate()
        .map(|(i, a)| if i == last { *a } else { a + dwell_s })
        .collect();
    Offsets { arrival, departure }
}

/// `(arrival, departure)` of each stop of a trip with reference time `ref_s`.
pub fn stop_times(ref_s: i32, offsets: &Offsets) -> Vec<(i32, i32)> {
    offsets
        .arrival
        .iter()
        .zip(&offsets.departure)
        .map(|(a, d)| (ref_s + a, ref_s + d))
        .collect()
}

/// The public id of a stop order, exactly as `gtfs_preprocessor.py` computes it:
/// `{g}:{route}:{md5(served stop ids joined by "|")[:8]}`. Never stored, so a DB
/// feed serves the ids the preprocessed one did, and editing a pattern's stops
/// changes its id as a rebuild would.
pub fn pattern_id<S: AsRef<str>>(gtfs_id: &str, route_id: &str, stop_ids: &[S]) -> String {
    let key = stop_ids
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join("|");
    let digest = Md5::digest(key.as_bytes());
    format!("{gtfs_id}:{route_id}:{}", &hex::encode(digest)[..8])
}

/// `H:MM:SS`, `HH:MM:SS` or `HH:MM` to seconds, up to 47:59:59. GTFS writes a
/// time past midnight as 24:00 and more.
pub fn parse_time(text: &str) -> Option<i32> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    let num = |p: &str, max: i32| -> Option<i32> {
        (!p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<i32>().ok())
            .flatten()
            .filter(|n| *n <= max)
    };
    let (h, m, s) = match parts.as_slice() {
        [h, m] => (num(h, 47)?, num(m, 59)?, 0),
        [h, m, s] => (num(h, 47)?, num(m, 59)?, num(s, 59)?),
        _ => return None,
    };
    // the minutes and seconds are always two digits; the hour may be one
    if parts[1].len() != 2 || parts.get(2).is_some_and(|s| s.len() != 2) {
        return None;
    }
    Some(h * 3600 + m * 60 + s)
}

/// The end of a frequency window: a time up to 47:59:59, or 48:00:00 itself -
/// a window lies within 00:00:00-48:00:00.
pub fn parse_window_end(text: &str) -> Option<i32> {
    match text.trim() {
        "48:00:00" | "48:00" => Some(MAX_TIME_S + 1),
        t => parse_time(t),
    }
}

/// Seconds to GTFS's `HH:MM:SS`, past 24:00 when it runs there (`26:07:15`).
pub fn format_time(seconds: i32) -> String {
    let s = seconds.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, s % 3600 / 60, s % 60)
}

/// Straight-line distance in metres.
pub fn haversine_m(a: (f64, f64), b: (f64, f64)) -> f64 {
    let (p1, p2) = (a.0.to_radians(), b.0.to_radians());
    let dp = (b.0 - a.0).to_radians();
    let dl = (b.1 - a.1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6_371_000.0 * h.sqrt().asin()
}

/// The first place offsets go backwards, as `(stop index, what)`: every stop
/// arrives no later than it departs, and departs no later than the next one
/// arrives.
pub fn goes_backwards(o: &Offsets) -> Option<(usize, &'static str)> {
    for i in 0..o.len() {
        if o.departure[i] < o.arrival[i] {
            return Some((i, "departs before it arrives"));
        }
        if i + 1 < o.len() && o.arrival[i + 1] < o.departure[i] {
            return Some((i + 1, "arrives before the stop before it departs"));
        }
    }
    None
}

/// Hops whose straight-line speed is implausible, as `(from stop index, km/h)`:
/// faster than [`IMPLAUSIBLE_FAST_KMH`] (a hop of no time over any distance
/// counts as infinitely fast) or slower than [`IMPLAUSIBLE_SLOW_KMH`]. A hop of
/// no time over no distance says nothing and is left alone.
pub fn implausible_hops(o: &Offsets, positions: &[(f64, f64)]) -> Vec<(usize, f64)> {
    let mut out = Vec::new();
    for i in 0..o
        .len()
        .saturating_sub(1)
        .min(positions.len().saturating_sub(1))
    {
        let metres = haversine_m(positions[i], positions[i + 1]);
        let seconds = o.arrival[i + 1] - o.departure[i];
        if seconds <= 0 {
            if metres > 0.0 {
                out.push((i, f64::INFINITY));
            }
            continue;
        }
        let kmh = metres / seconds as f64 * 3.6;
        if !(IMPLAUSIBLE_SLOW_KMH..=IMPLAUSIBLE_FAST_KMH).contains(&kmh) {
            out.push((i, kmh));
        }
    }
    out
}

/// A profile carried over onto a changed stop list, and how many of its stops
/// had to be estimated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedOver {
    pub offsets: Offsets,
    pub estimated: usize,
}

/// Index pairs `(old, new)` of the stops a new stop list keeps from the old one,
/// matched in order by stop id: the longest common subsequence, so a stop a
/// loop calls at twice is matched twice, in its places.
pub fn kept_stops<S: AsRef<str>, T: AsRef<str>>(old: &[S], new: &[T]) -> Vec<(usize, usize)> {
    let (n, m) = (old.len(), new.len());
    let mut len = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            len[i][j] = if old[i].as_ref() == new[j].as_ref() {
                len[i + 1][j + 1] + 1
            } else {
                len[i + 1][j].max(len[i][j + 1])
            };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, Vec::new());
    while i < n && j < m {
        if old[i].as_ref() == new[j].as_ref() {
            out.push((i, j));
            i += 1;
            j += 1;
        } else if len[i + 1][j] >= len[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// A pattern's median dwell, from a profile: what an inserted stop waits.
fn median_dwell(o: &Offsets) -> i32 {
    // the last stop's dwell is 0 by convention, not a measurement
    let mut dwells: Vec<i32> = (0..o.len().saturating_sub(1))
        .map(|i| (o.departure[i] - o.arrival[i]).max(0))
        .collect();
    if dwells.is_empty() {
        return 0;
    }
    dwells.sort_unstable();
    dwells[(dwells.len() - 1) / 2]
}

/// The offsets of a profile once its pattern's served stops change from
/// `old_ids` to `new_ids` (16.4, "Stop lists and stored timings"):
///
/// - stops kept (matched in order by stop id, [`kept_stops`]) keep their offsets;
/// - an inserted stop between two kept ones arrives at a time interpolated by
///   straight-line distance between the first's departure and the second's
///   arrival, and waits the pattern's median dwell;
/// - one inserted before the first or after the last kept stop is placed at the
///   speed of the nearest kept hop;
/// - a removed stop's offsets go. Removing the first stop leaves the rest where
///   they were, so every trip keeps its clock times: the reference time is not
///   a start time (16.3).
///
/// `new_positions` are the new stops' positions. `None` when `old` does not have
/// one offset per old stop.
pub fn carry_over<S: AsRef<str>, T: AsRef<str>>(
    old_ids: &[S],
    old: &Offsets,
    new_ids: &[T],
    new_positions: &[(f64, f64)],
) -> Option<CarriedOver> {
    if old.len() != old_ids.len() || new_positions.len() != new_ids.len() {
        return None;
    }
    let n = new_ids.len();
    let kept = kept_stops(old_ids, new_ids);
    let dwell = median_dwell(old);
    let mut arrival: Vec<Option<i32>> = vec![None; n];
    let mut departure: Vec<Option<i32>> = vec![None; n];
    for &(o, k) in &kept {
        arrival[k] = Some(old.arrival[o]);
        departure[k] = Some(old.departure[o]);
    }
    let dist = |a: usize, b: usize| haversine_m(new_positions[a], new_positions[b]);
    let kept_new: Vec<usize> = kept.iter().map(|&(_, k)| k).collect();
    // metres per second over each kept hop that takes time and covers ground,
    // measured before anything is inserted between them
    let hop_speeds: Vec<f64> = kept_new
        .windows(2)
        .filter_map(|w| {
            let seconds = arrival[w[1]]? - departure[w[0]]?;
            let metres = dist(w[0], w[1]);
            (seconds > 0 && metres > 0.0).then(|| metres / seconds as f64)
        })
        .collect();
    // the speed of the whole old run, for when no kept hop is left to measure
    let overall = {
        let path: f64 = kept_new.windows(2).map(|w| dist(w[0], w[1])).sum();
        match (kept_new.first(), kept_new.last()) {
            (Some(&a), Some(&b)) if b > a => {
                let seconds = arrival[b].unwrap_or(0) - departure[a].unwrap_or(0);
                (seconds > 0 && path > 0.0).then(|| path / seconds as f64)
            }
            _ => None,
        }
    }
    .unwrap_or(FALLBACK_SPEED_MPS);
    let first_speed = hop_speeds.first().copied().unwrap_or(overall);
    let last_speed = hop_speeds.last().copied().unwrap_or(overall);

    let mut estimated = 0;
    // between two kept stops
    for w in kept_new.windows(2) {
        let (a, b) = (w[0], w[1]);
        if b == a + 1 {
            continue;
        }
        let inserted = b - a - 1;
        estimated += inserted;
        let (from, to) = (departure[a].unwrap_or(0), arrival[b].unwrap_or(0));
        let available = (to - from).max(0);
        let wait = if dwell * inserted as i32 <= available {
            dwell
        } else {
            0
        };
        let running = available - wait * inserted as i32;
        let legs: Vec<f64> = (a..b).map(|i| dist(i, i + 1)).collect();
        let total: f64 = legs.iter().sum();
        let mut covered = 0.0;
        for (step, i) in (a + 1..b).enumerate() {
            covered += legs[step];
            let share = if total > 0.0 {
                covered / total
            } else {
                (step + 1) as f64 / (inserted + 1) as f64
            };
            let at = from + (running as f64 * share).round() as i32 + wait * step as i32;
            arrival[i] = Some(at);
            departure[i] = Some(at + wait);
        }
    }
    // before the first kept stop, at the speed of the first kept hop, backwards
    if let Some(&first) = kept_new.first() {
        for i in (0..first).rev() {
            estimated += 1;
            let next_arrival = arrival[i + 1].unwrap_or(0);
            let run = (dist(i, i + 1) / first_speed).round() as i32;
            departure[i] = Some(next_arrival - run);
            arrival[i] = Some(next_arrival - run - dwell);
        }
    }
    // after the last kept stop, at the speed of the last kept hop, forwards
    let start = kept_new.last().map(|&k| k + 1).unwrap_or(0);
    for i in start..n {
        estimated += 1;
        let at = if i == 0 {
            0
        } else {
            departure[i - 1].unwrap_or(0) + (dist(i - 1, i) / last_speed).round() as i32
        };
        arrival[i] = Some(at);
        departure[i] = Some(at + dwell);
    }
    // the last stop departs when it arrives, whether it was kept or not
    if let (Some(a), Some(d)) = (arrival.last().copied().flatten(), departure.last_mut()) {
        if start < n {
            *d = Some(a);
        }
    }
    Some(CarriedOver {
        offsets: Offsets {
            arrival: arrival.into_iter().map(|a| a.unwrap_or(0)).collect(),
            departure: departure.into_iter().map(|d| d.unwrap_or(0)).collect(),
        },
        estimated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `generate_trips_from_db.py` writes for a trip starting at `start`
    /// over `stops` stops, formatted as its `stop_times.txt` does.
    fn generator(start: i32, stops: usize) -> Vec<(String, String)> {
        let last = stops - 1;
        (0..stops)
            .map(|i| {
                let arrival = start + 135 * i as i32;
                let departure = if i == last { arrival } else { arrival + 15 };
                (format_time(arrival), format_time(departure))
            })
            .collect()
    }

    #[test]
    fn the_default_timing_is_the_generators_formula_to_the_second() {
        // 03:40 is the earliest chennai_bus departure, 23:55 the latest, and a
        // 90-stop route runs past midnight: every one of them byte for byte
        for (start, stops) in [(13200, 2), (13200, 17), (21600, 45), (86100, 90)] {
            let offsets = default_offsets(stops, 120, 15);
            let served: Vec<(String, String)> = stop_times(start, &offsets)
                .into_iter()
                .map(|(a, d)| (format_time(a), format_time(d)))
                .collect();
            assert_eq!(
                served,
                generator(start, stops),
                "start {start}, {stops} stops"
            );
        }
        assert_eq!(
            default_offsets(3, 120, 15),
            Offsets {
                arrival: vec![0, 135, 270],
                departure: vec![15, 150, 270]
            }
        );
        // the feed's own defaults, when it changes them
        assert_eq!(default_offsets(2, 100, 20).departure, vec![20, 120]);
        assert!(default_offsets(0, 120, 15).is_empty());
    }

    #[test]
    fn a_patterns_id_is_the_preprocessors() {
        // hashlib.md5("|".join(ids).encode()).hexdigest()[:8], computed in
        // Python; the first two are ids the preprocessed data serves today
        assert_eq!(
            pattern_id(
                "chennai_bus",
                "3",
                &[
                    "ceb46e7bbf",
                    "9e08324366",
                    "3b9a2c850f",
                    "4210b6bd61",
                    "5c129191f7",
                    "0d7f8e8568",
                    "4bd3d161c5",
                    "abdf0ebe40",
                    "fd18ed695a",
                    "a303122152",
                    "d97f8fa522",
                    "68d30512ce",
                    "8f7fd6a163",
                ]
            ),
            "chennai_bus:3:9960a460"
        );
        assert_eq!(
            pattern_id(
                "kochi_metro",
                "BLUE_D",
                &[
                    "wzSo8ZMX6FEbJzmvVcftl6KKXoiW4Y",
                    "8m3SWvdtmpEh16IziODHdkb0nUo8qU",
                    "uiz4ZGRq9tFZFysnc78pO56Kg6PNJM",
                    "zOB3jfWkWYeNMGdL6P6KZ8bC6TubO4",
                    "nMgcLugnZBOEewOjFeDSu1pGl36jYm",
                ]
            ),
            "kochi_metro:BLUE_D:ead52993"
        );
        assert_eq!(
            pattern_id(
                "chennai_bus",
                "3",
                &["ceb46e7bbf", "9e08324366", "3b9a2c850f"]
            ),
            "chennai_bus:3:2ae68b67"
        );
        // an id that holds the separator is hashed as it is, like Python does
        assert_eq!(pattern_id("g", "r", &["SWA|0101"]), "g:r:d76dd1f0");
        let none: [&str; 0] = [];
        assert_eq!(pattern_id("g", "r", &none), "g:r:d41d8cd9");
    }

    #[test]
    fn gtfs_times_run_past_midnight() {
        assert_eq!(parse_time("26:07:15"), Some(94035));
        assert_eq!(parse_time("5:10:00"), Some(18600));
        assert_eq!(parse_time("05:10"), Some(18600));
        assert_eq!(parse_time("47:59:59"), Some(MAX_TIME_S));
        for bad in [
            "48:00:00", "5.10", "05:1", "05:60", "", "1:2:3:4", "-1:00", "aa:bb",
        ] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(parse_window_end("48:00:00"), Some(172800));
        assert_eq!(parse_window_end("25:00:00"), Some(90000));
        assert_eq!(parse_window_end("48:00:01"), None);
        assert_eq!(format_time(94035), "26:07:15");
        assert_eq!(format_time(0), "00:00:00");
    }

    #[test]
    fn offsets_that_go_backwards_are_found() {
        let o = |a: &[i32], d: &[i32]| Offsets {
            arrival: a.to_vec(),
            departure: d.to_vec(),
        };
        assert_eq!(goes_backwards(&o(&[0, 100], &[10, 100])), None);
        assert_eq!(goes_backwards(&o(&[0, 100], &[10, 90])).unwrap().0, 1);
        assert_eq!(goes_backwards(&o(&[0, 5], &[10, 5])).unwrap().0, 1);
        // a stop may depart the moment the one before it does
        assert_eq!(goes_backwards(&o(&[0, 10], &[10, 10])), None);
    }

    #[test]
    fn implausible_hops_are_the_too_fast_and_the_too_slow() {
        // ~1.1 km apart
        let pos = [(13.0, 80.2), (13.01, 80.2), (13.02, 80.2), (13.02, 80.2)];
        let o = Offsets {
            // 120 s (33 km/h), 10 s (400 km/h), 0 s over 0 m
            arrival: vec![0, 120, 130, 130],
            departure: vec![0, 120, 130, 130],
        };
        let found = implausible_hops(&o, &pos);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, 1);
        let crawl = Offsets {
            arrival: vec![0, 3600],
            departure: vec![0, 3600],
        };
        assert_eq!(implausible_hops(&crawl, &pos[..2])[0].0, 0);
    }

    fn at(lat: f64) -> (f64, f64) {
        (lat, 80.2)
    }

    #[test]
    fn kept_stops_keep_their_offsets_and_a_removed_first_stop_keeps_the_clock() {
        let old = Offsets {
            arrival: vec![0, 300, 700, 1000],
            departure: vec![30, 330, 730, 1000],
        };
        // B dropped from the middle, A dropped from the front
        let c = carry_over(
            &["A", "B", "C", "D"],
            &old,
            &["C", "D"],
            &[at(13.02), at(13.03)],
        )
        .unwrap();
        assert_eq!(c.estimated, 0);
        assert_eq!(
            c.offsets,
            Offsets {
                arrival: vec![700, 1000],
                departure: vec![730, 1000]
            }
        );
        // a loop: the same stop twice is matched twice, in its places
        assert_eq!(
            kept_stops(&["A", "B", "A"], &["A", "B", "X", "A"]),
            vec![(0, 0), (1, 1), (2, 3)]
        );
    }

    #[test]
    fn an_inserted_stop_is_placed_by_distance_with_the_median_dwell() {
        let old = Offsets {
            arrival: vec![0, 600],
            departure: vec![20, 600],
        };
        // X a quarter of the way from A to B: 580 s available, 20 s of dwell,
        // 560 s of running, a quarter of which is 140 s
        let c = carry_over(
            &["A", "B"],
            &old,
            &["A", "X", "B"],
            &[at(13.0), at(13.0025), at(13.01)],
        )
        .unwrap();
        assert_eq!(c.estimated, 1);
        assert_eq!(c.offsets.arrival, vec![0, 160, 600]);
        assert_eq!(c.offsets.departure, vec![20, 180, 600]);
        assert_eq!(goes_backwards(&c.offsets), None);
    }

    #[test]
    fn a_stop_added_at_either_end_runs_at_the_nearest_hops_speed() {
        // A -> B: 1.11 km in 100 s after a 20 s dwell
        let old = Offsets {
            arrival: vec![0, 120],
            departure: vec![20, 120],
        };
        let c = carry_over(
            &["A", "B"],
            &old,
            &["P", "A", "B", "Q"],
            &[at(12.99), at(13.0), at(13.01), at(13.02)],
        )
        .unwrap();
        assert_eq!(c.estimated, 2);
        // P departs 100 s before A arrives, and waits the median dwell (20 s)
        assert_eq!(c.offsets.arrival[0], -120);
        assert_eq!(c.offsets.departure[0], -100);
        // A and B keep theirs; B, no longer last, keeps its own departure
        assert_eq!(&c.offsets.arrival[1..3], &[0, 120]);
        // Q is reached 100 s after B leaves, and departs when it arrives
        assert_eq!(c.offsets.arrival[3], 220);
        assert_eq!(c.offsets.departure[3], 220);
        assert_eq!(goes_backwards(&c.offsets), None);
    }

    #[test]
    fn a_profile_that_does_not_fit_its_stops_is_not_carried() {
        let old = Offsets {
            arrival: vec![0],
            departure: vec![0],
        };
        assert!(carry_over(&["A", "B"], &old, &["A"], &[at(13.0)]).is_none());
    }
}
