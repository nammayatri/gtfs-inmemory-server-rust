//! A route's map line and a GTFS zip's shapes (docs/gtfs-editor.md section 32).
//!
//! GIMS keeps one map line a route (`gtfs_route.encoded_polyline`); GTFS keeps
//! shapes, which trips name (`trips.shape_id`). The two meet here:
//!
//! - **in** (a seed): a route's line is its *main shape* - the shape most of
//!   its trips run, the first of them on a tie - or, for a route with no
//!   trips, a shape that has the route's id and that no trip runs. It is kept
//!   with `polyline_source = 'imported'`.
//! - **out** (the zip a feed gives, downloaded or published): a line drawn in
//!   the editor (by the road router, from GPS, by hand; anything but
//!   `imported`) is written into `shapes.txt`, as the route's main shape:
//!   - that shape, when the route's trips are the only ones to run it, takes
//!     the line's points;
//!   - a shape the trips name that the feed does not keep is made from it;
//!   - a shape other routes run too stays theirs, and the route's trips that
//!     ran it run a new one;
//!   - a route whose trips run no shape gets one, and its trips run it (a
//!     route with no trips: a shape with its id, as it is read back in).
//!
//!   An imported line leaves the shapes as they are when the route's main
//!   shape is there: that shape is what it came from (or has been edited
//!   since, as a record), so a zip seeded and exported again is the same zip.
//!   An imported line whose shape the feed does not keep - an import of
//!   `routes.txt` without `shapes.txt` - is written as a drawn one is. The
//!   route's other shapes (a short working's, say) are left as they are.

use crate::gtfs::model::{FeedModel, Shape, ShapePoint};
use crate::services::osrm;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Where a line came from, when it came from a zip's shapes.
pub const IMPORTED: &str = "imported";

/// One route's map line as the feed keeps it.
#[derive(Debug, Clone)]
pub struct RouteLine {
    pub route_id: String,
    pub points: Vec<(f64, f64)>,
    pub source: Option<String>,
}

/// What [`lines_into_shapes`] did, for the export's report.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct LinesReport {
    /// Routes whose drawn line is now their main shape.
    pub routes: usize,
    pub shapes_added: usize,
    pub shapes_redrawn: usize,
    /// Trips that now run a shape they did not.
    pub trips_repointed: usize,
}

fn trip_shape(values: &crate::gtfs::model::Row) -> Option<String> {
    values
        .get("shape_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// What [`lines_from_shapes`] and [`lines_into_shapes`] look up, read in one
/// pass over the trips: a feed has thousands of routes and tens of thousands
/// of trips, so nothing here walks the trips once per route.
struct Index {
    /// Each route's trips, as indexes into the model's trips.
    trips_of: HashMap<String, Vec<usize>>,
    /// Each route's main shape.
    main: HashMap<String, String>,
    /// Each shape run by a trip: the routes whose trips run it, and how many.
    runners: HashMap<String, HashMap<String, usize>>,
    /// Each shape the model keeps: where it is.
    shape_at: HashMap<String, usize>,
    /// Every shape id kept or named by a trip.
    taken: HashSet<String>,
}

fn index(m: &FeedModel) -> Index {
    let mut trips_of: HashMap<String, Vec<usize>> = HashMap::new();
    let mut runners: HashMap<String, HashMap<String, usize>> = HashMap::new();
    // (route, shape) -> (trips, the first of them)
    let mut counts: HashMap<(String, String), (usize, usize)> = HashMap::new();
    for (i, t) in m.trips.iter().enumerate() {
        trips_of.entry(t.route_id.clone()).or_default().push(i);
        if let Some(s) = trip_shape(&t.values) {
            counts
                .entry((t.route_id.clone(), s.clone()))
                .or_insert((0, i))
                .0 += 1;
            *runners
                .entry(s)
                .or_default()
                .entry(t.route_id.clone())
                .or_default() += 1;
        }
    }
    // the most trips; on a tie, the shape run first
    let mut best: HashMap<String, (String, usize, usize)> = HashMap::new();
    for ((route, shape), (n, first)) in counts {
        let better = match best.get(&route) {
            Some((_, bn, bf)) => n > *bn || (n == *bn && first < *bf),
            None => true,
        };
        if better {
            best.insert(route, (shape, n, first));
        }
    }
    let shape_at: HashMap<String, usize> = m
        .shapes
        .iter()
        .enumerate()
        .map(|(i, s)| (s.shape_id.clone(), i))
        .collect();
    let taken = shape_at.keys().chain(runners.keys()).cloned().collect();
    Index {
        trips_of,
        main: best.into_iter().map(|(r, (s, _, _))| (r, s)).collect(),
        runners,
        shape_at,
        taken,
    }
}

/// The shape most of a route's trips run, the first of them on a tie.
pub fn main_shape(m: &FeedModel, route_id: &str) -> Option<String> {
    index(m).main.remove(route_id)
}

/// A shape's points in its order, as (lat, lon).
pub fn shape_line(s: &Shape) -> Vec<(f64, f64)> {
    let mut pts: Vec<&ShapePoint> = s.points.iter().collect();
    pts.sort_by_key(|p| p.sequence);
    pts.iter().map(|p| (p.lat, p.lon)).collect()
}

/// Two lines are one when they encode the same: a map line keeps 1e-5 of a
/// degree, so a shape is its route's line when it rounds to it.
fn same(a: &[(f64, f64)], b: &[(f64, f64)]) -> bool {
    osrm::encode_polyline(a) == osrm::encode_polyline(b)
}

/// In: every route's line from the zip's shapes. Routes with none are left out.
pub fn lines_from_shapes(m: &FeedModel) -> HashMap<String, Vec<(f64, f64)>> {
    let idx = index(m);
    let mut out = HashMap::new();
    for r in &m.routes {
        let id = idx.main.get(&r.route_id).cloned().or_else(|| {
            // a route with no trips: the shape named for it that no trip runs
            (!idx.trips_of.contains_key(&r.route_id) && !idx.runners.contains_key(&r.route_id))
                .then(|| r.route_id.clone())
        });
        let Some(&i) = id.and_then(|id| idx.shape_at.get(&id)) else {
            continue;
        };
        let line = shape_line(&m.shapes[i]);
        if line.len() >= 2 {
            out.insert(r.route_id.clone(), line);
        }
    }
    out
}

fn points_of(line: &[(f64, f64)]) -> Vec<ShapePoint> {
    line.iter()
        .enumerate()
        .map(|(i, &(lat, lon))| ShapePoint {
            sequence: i as i64 + 1,
            lat,
            lon,
            dist: None,
        })
        .collect()
}

/// A shape id no shape and no trip has yet: the route's id, else with a suffix.
fn fresh_id(taken: &HashSet<String>, route_id: &str) -> String {
    if !taken.contains(route_id) {
        return route_id.to_string();
    }
    (2..)
        .map(|n| format!("{route_id}_line{n}"))
        .find(|id| !taken.contains(id))
        .expect("a free id")
}

/// Out: the lines drawn in the editor into the model's shapes, as the module
/// says. Lines of routes the model does not have (deleted, or left out of a
/// published zip) are left out.
pub fn lines_into_shapes(m: &mut FeedModel, lines: &[RouteLine]) -> LinesReport {
    let mut report = LinesReport::default();
    let routes: HashSet<String> = m.routes.iter().map(|r| r.route_id.clone()).collect();
    let mut idx = index(m);
    for l in lines {
        if l.points.len() < 2 || !routes.contains(&l.route_id) {
            continue;
        }
        let route = l.route_id.as_str();
        let main = idx.main.get(route).cloned();
        let has_trips = idx.trips_of.contains_key(route);
        // an imported line whose shape is there: the shape is the truth
        if l.source.as_deref() == Some(IMPORTED) {
            let from = main
                .clone()
                .or_else(|| (!has_trips).then(|| route.to_string()));
            if from.is_some_and(|id| idx.shape_at.contains_key(&id)) {
                continue;
            }
        }
        // which shape becomes the line, and which trips run it
        let (target, repoint): (String, Option<Option<String>>) = match &main {
            None => (fresh_id(&idx.taken, route), Some(None)),
            Some(sid) => {
                let others = idx
                    .runners
                    .get(sid)
                    .is_some_and(|rs| rs.keys().any(|r| r != route));
                if others {
                    (fresh_id(&idx.taken, route), Some(Some(sid.clone())))
                } else {
                    (sid.clone(), None)
                }
            }
        };
        match idx.shape_at.get(&target) {
            Some(&i) if same(&shape_line(&m.shapes[i]), &l.points) => continue,
            Some(&i) => {
                m.shapes[i].points = points_of(&l.points);
                report.shapes_redrawn += 1;
            }
            None => {
                m.shapes.push(Shape {
                    shape_id: target.clone(),
                    points: points_of(&l.points),
                });
                idx.shape_at.insert(target.clone(), m.shapes.len() - 1);
                report.shapes_added += 1;
            }
        }
        idx.taken.insert(target.clone());
        report.routes += 1;
        // the trips that ran nothing (no main shape), or the shared one
        let Some(from) = repoint.filter(|_| has_trips) else {
            continue;
        };
        for &i in idx.trips_of.get(route).map(Vec::as_slice).unwrap_or(&[]) {
            if trip_shape(&m.trips[i].values) != from {
                continue;
            }
            m.trips[i].values.insert("shape_id", json!(target));
            report.trips_repointed += 1;
            // a later route sees who runs what now
            if let Some(f) = &from {
                if let Some(rs) = idx.runners.get_mut(f) {
                    if let Some(n) = rs.get_mut(route) {
                        *n -= 1;
                        if *n == 0 {
                            rs.remove(route);
                        }
                    }
                }
            }
            *idx.runners
                .entry(target.clone())
                .or_default()
                .entry(route.to_string())
                .or_default() += 1;
        }
    }
    report
}

/// The feed's lines as [`lines_into_shapes`] takes them.
pub async fn feed_lines(
    conn: &mut sqlx::PgConnection,
    g: &str,
) -> Result<Vec<RouteLine>, sqlx::Error> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT route_id, encoded_polyline, polyline_source FROM gtfs_route \
         WHERE gtfs_id = $1 AND NOT deleted AND encoded_polyline IS NOT NULL AND encoded_polyline <> '' \
         ORDER BY route_id",
    )
    .bind(g)
    .fetch_all(&mut *conn)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let line: String = r.try_get("encoded_polyline")?;
        let Some(points) = osrm::decode_polyline(&line) else {
            continue;
        };
        out.push(RouteLine {
            route_id: r.try_get("route_id")?,
            points,
            source: r.try_get("polyline_source")?,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::model::{Route, Row, Trip};

    fn route(id: &str) -> Route {
        Route {
            route_id: id.into(),
            values: Row::new(),
            sort_key: None,
        }
    }

    fn trip(id: &str, route: &str, shape: Option<&str>) -> Trip {
        let mut values = Row::new();
        if let Some(s) = shape {
            values.insert("shape_id", json!(s));
        }
        Trip {
            trip_id: id.into(),
            route_id: route.into(),
            service_id: "WK".into(),
            pattern_key: 1,
            profile_key: None,
            ref_s: 0,
            values,
            frequencies: vec![],
            sort_key: 0,
        }
    }

    fn shape(id: &str, pts: &[(f64, f64)]) -> Shape {
        Shape {
            shape_id: id.into(),
            points: points_of(pts),
        }
    }

    const A: [(f64, f64); 3] = [(13.0, 80.2), (13.001, 80.2), (13.002, 80.201)];
    const B: [(f64, f64); 2] = [(13.1, 80.3), (13.101, 80.3)];

    fn line(route: &str, pts: &[(f64, f64)], source: &str) -> RouteLine {
        RouteLine {
            route_id: route.into(),
            points: pts.to_vec(),
            source: Some(source.into()),
        }
    }

    fn shape_of<'a>(m: &'a FeedModel, id: &str) -> Option<&'a Shape> {
        m.shapes.iter().find(|s| s.shape_id == id)
    }

    fn trip_shapes(m: &FeedModel, route: &str) -> Vec<Option<String>> {
        m.trips
            .iter()
            .filter(|t| t.route_id == route)
            .map(|t| trip_shape(&t.values))
            .collect()
    }

    #[test]
    fn the_main_shape_is_the_one_most_trips_run() {
        let m = FeedModel {
            trips: vec![
                trip("t1", "R", Some("short")),
                trip("t2", "R", Some("long")),
                trip("t3", "R", Some("long")),
                trip("t4", "R", None),
                trip("t5", "Q", Some("short")),
                trip("t6", "Q", Some("short")),
                trip("t7", "P", Some("x")),
                trip("t8", "P", Some("y")),
            ],
            ..Default::default()
        };
        assert_eq!(main_shape(&m, "R").as_deref(), Some("long"));
        // a tie: the first run
        assert_eq!(main_shape(&m, "P").as_deref(), Some("x"));
        assert_eq!(main_shape(&m, "none"), None);
    }

    #[test]
    fn in_a_route_takes_its_main_shape_or_the_shape_named_for_it() {
        let mut points = points_of(&A);
        points.reverse(); // out of order in the file: the sequence orders them
        let m = FeedModel {
            routes: vec![route("R"), route("Q"), route("NOTRIPS"), route("NOSHAPE")],
            trips: vec![
                trip("t1", "R", Some("SA")),
                trip("t2", "Q", Some("missing")),
                trip("t3", "NOSHAPE", None),
            ],
            shapes: vec![
                Shape {
                    shape_id: "SA".into(),
                    points,
                },
                shape("NOTRIPS", &B),
            ],
            ..Default::default()
        };
        let lines = lines_from_shapes(&m);
        assert_eq!(lines.get("R"), Some(&A.to_vec()));
        assert_eq!(lines.get("NOTRIPS"), Some(&B.to_vec()));
        assert!(!lines.contains_key("Q"), "a shape the zip does not have");
        assert!(!lines.contains_key("NOSHAPE"));
    }

    #[test]
    fn out_an_imported_line_leaves_the_shapes_as_they_are() {
        let mut m = FeedModel {
            routes: vec![route("R")],
            trips: vec![trip("t1", "R", Some("SA"))],
            shapes: vec![shape("SA", &B)],
            ..Default::default()
        };
        let before = m.clone();
        let r = lines_into_shapes(&mut m, &[line("R", &A, IMPORTED)]);
        assert_eq!(r, LinesReport::default());
        assert_eq!(m, before);
        // a drawn line that is the shape already: nothing either
        let r = lines_into_shapes(&mut m, &[line("R", &B, "manual")]);
        assert_eq!(r, LinesReport::default());
        assert_eq!(m, before);
    }

    #[test]
    fn out_an_imported_line_whose_shape_the_feed_lacks_is_written() {
        // routes.txt imported without shapes.txt: the line came, its shape did not
        let mut m = FeedModel {
            routes: vec![route("R"), route("EMPTY")],
            trips: vec![trip("t1", "R", None)],
            ..Default::default()
        };
        let r = lines_into_shapes(
            &mut m,
            &[line("R", &A, IMPORTED), line("EMPTY", &B, IMPORTED)],
        );
        assert_eq!(
            r,
            LinesReport {
                routes: 2,
                shapes_added: 2,
                trips_repointed: 1,
                ..Default::default()
            }
        );
        assert_eq!(trip_shapes(&m, "R"), vec![Some("R".into())]);
        assert_eq!(shape_line(shape_of(&m, "EMPTY").unwrap()), B.to_vec());
    }

    #[test]
    fn out_a_drawn_line_redraws_the_shape_only_its_route_runs() {
        let mut m = FeedModel {
            routes: vec![route("R")],
            trips: vec![
                trip("t1", "R", Some("SA")),
                trip("t2", "R", Some("SA")),
                trip("t3", "R", Some("short")),
            ],
            shapes: vec![shape("SA", &B), shape("short", &B[..])],
            ..Default::default()
        };
        let r = lines_into_shapes(&mut m, &[line("R", &A, "manual")]);
        assert_eq!(
            r,
            LinesReport {
                routes: 1,
                shapes_redrawn: 1,
                ..Default::default()
            }
        );
        assert_eq!(shape_line(shape_of(&m, "SA").unwrap()), A.to_vec());
        assert!(shape_of(&m, "SA")
            .unwrap()
            .points
            .iter()
            .all(|p| p.dist.is_none()));
        assert_eq!(
            shape_line(shape_of(&m, "short").unwrap()),
            B.to_vec(),
            "its other shapes stay"
        );
        assert_eq!(
            trip_shapes(&m, "R"),
            vec![Some("SA".into()), Some("SA".into()), Some("short".into())]
        );
        // and it is now as the line: a second export changes nothing
        let again = m.clone();
        assert_eq!(
            lines_into_shapes(&mut m, &[line("R", &A, "manual")]),
            LinesReport::default()
        );
        assert_eq!(m, again);
    }

    #[test]
    fn out_a_shape_other_routes_run_stays_theirs() {
        let mut m = FeedModel {
            routes: vec![route("R"), route("Q")],
            trips: vec![trip("t1", "R", Some("SA")), trip("t2", "Q", Some("SA"))],
            shapes: vec![shape("SA", &B), shape("R", &B)],
            ..Default::default()
        };
        let r = lines_into_shapes(&mut m, &[line("R", &A, "gps")]);
        assert_eq!(
            r,
            LinesReport {
                routes: 1,
                shapes_added: 1,
                trips_repointed: 1,
                ..Default::default()
            }
        );
        assert_eq!(
            shape_line(shape_of(&m, "SA").unwrap()),
            B.to_vec(),
            "Q's shape untouched"
        );
        // the route's id is taken by a shape: a suffix
        assert_eq!(trip_shapes(&m, "R"), vec![Some("R_line2".into())]);
        assert_eq!(shape_line(shape_of(&m, "R_line2").unwrap()), A.to_vec());
        assert_eq!(trip_shapes(&m, "Q"), vec![Some("SA".into())]);
    }

    #[test]
    fn out_a_shape_named_but_not_kept_is_made_from_the_line() {
        // chennai_bus: trips name shapes the feed does not keep
        let mut m = FeedModel {
            routes: vec![route("R")],
            trips: vec![trip("t1", "R", Some("21G-UP"))],
            ..Default::default()
        };
        let r = lines_into_shapes(&mut m, &[line("R", &A, "gps")]);
        assert_eq!(
            r,
            LinesReport {
                routes: 1,
                shapes_added: 1,
                ..Default::default()
            }
        );
        assert_eq!(shape_line(shape_of(&m, "21G-UP").unwrap()), A.to_vec());
        assert_eq!(trip_shapes(&m, "R"), vec![Some("21G-UP".into())]);
    }

    #[test]
    fn out_a_route_with_no_shape_gets_one_and_reads_back_in() {
        let mut m = FeedModel {
            routes: vec![route("R"), route("EMPTY"), route("OTHER")],
            trips: vec![trip("t1", "R", None), trip("t2", "R", None)],
            ..Default::default()
        };
        let lines = [
            line("R", &A, "osrm"),
            line("EMPTY", &B, "manual"),
            // a route the zip leaves out (deleted, or inactive in a published zip)
            line("GONE", &A, "manual"),
        ];
        let r = lines_into_shapes(&mut m, &lines);
        assert_eq!(
            r,
            LinesReport {
                routes: 2,
                shapes_added: 2,
                trips_repointed: 2,
                ..Default::default()
            }
        );
        assert_eq!(
            trip_shapes(&m, "R"),
            vec![Some("R".into()), Some("R".into())]
        );
        assert!(shape_of(&m, "GONE").is_none());
        // and in again: the same lines
        let back = lines_from_shapes(&m);
        assert_eq!(back.get("R"), Some(&A.to_vec()));
        assert_eq!(back.get("EMPTY"), Some(&B.to_vec()));
        assert!(!back.contains_key("OTHER"));
    }

    #[test]
    fn a_feed_the_size_of_chennai_bus_is_quick() {
        // 5,000 routes of 10 trips each, every route with a line drawn and
        // trips naming shapes the feed does not keep: one pass, not one a route
        let n = 5_000;
        let mut m = FeedModel::default();
        let mut lines = Vec::new();
        for r in 0..n {
            let id = format!("R{r}");
            m.routes.push(route(&id));
            for t in 0..10 {
                m.trips
                    .push(trip(&format!("T{r}-{t}"), &id, Some(&format!("S{r}"))));
            }
            lines.push(line(&id, &A, "gps"));
        }
        let started = std::time::Instant::now();
        let r = lines_into_shapes(&mut m, &lines);
        let took = started.elapsed();
        assert_eq!((r.routes, r.shapes_added), (n, n));
        let back = lines_from_shapes(&m);
        assert_eq!(back.len(), n);
        assert!(took.as_secs_f64() < 5.0, "took {took:?}");
    }
}
