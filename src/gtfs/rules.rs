//! The rules of the GTFS reference that one row can break on its own: fields
//! that are required, or forbidden, depending on the row's other fields. The
//! spec registry says which fields are "conditionally required"; this is the
//! condition. Used when a record change applies (`editor::records`) and by the
//! feed report, which adds the rules that need the whole feed.
//!
//! Every value is canonical ([`super::spec::Canon`]): times in seconds, dates
//! ISO, enums as numbers.

use super::model::Row;
use serde_json::Value;

/// One broken rule: whether it blocks, a code, the field it is about.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleFinding {
    pub error: bool,
    pub code: &'static str,
    pub field: &'static str,
    pub message: String,
}

fn err(code: &'static str, field: &'static str, message: impl Into<String>) -> RuleFinding {
    RuleFinding {
        error: true,
        code,
        field,
        message: message.into(),
    }
}

fn has(row: &Row, f: &str) -> bool {
    row.get(f).is_some_and(|v| !v.is_null())
}

fn int(row: &Row, f: &str) -> Option<i64> {
    row.get(f).and_then(Value::as_i64)
}

fn text<'a>(row: &'a Row, f: &str) -> Option<&'a str> {
    row.get(f).and_then(Value::as_str)
}

/// `a` and `b` are given together or not at all.
fn together(row: &Row, a: &'static str, b: &'static str, out: &mut Vec<RuleFinding>) {
    if has(row, a) != has(row, b) {
        let (given, missing) = if has(row, a) { (a, b) } else { (b, a) };
        out.push(err(
            "fields_go_together",
            missing,
            format!("{a} and {b} are given together; this row has {given} and no {missing}"),
        ));
    }
}

/// What one row of `file` breaks on its own.
pub fn row_findings(file: &str, row: &Row) -> Vec<RuleFinding> {
    let mut out = Vec::new();
    match file {
        "stops.txt" => {
            let lt = int(row, "location_type").unwrap_or(0);
            if lt <= 2 && !has(row, "stop_name") {
                out.push(err(
                    "missing_field",
                    "stop_name",
                    format!("a stop of location type {lt} needs a stop_name"),
                ));
            }
            match lt {
                1 if has(row, "parent_station") => out.push(err(
                    "station_with_parent",
                    "parent_station",
                    "a station cannot have a parent station",
                )),
                2..=4 if !has(row, "parent_station") => out.push(err(
                    "missing_field",
                    "parent_station",
                    format!(
                        "{} needs a parent_station",
                        match lt {
                            2 => "an entrance or exit",
                            3 => "a generic node",
                            _ => "a boarding area",
                        }
                    ),
                )),
                _ => {}
            }
            if has(row, "stop_access") && !has(row, "parent_station") {
                out.push(err(
                    "field_forbidden",
                    "stop_access",
                    "stop_access is only for a stop that has a parent station",
                ));
            }
        }
        "routes.txt" if !has(row, "route_short_name") && !has(row, "route_long_name") => {
            out.push(err(
                "missing_field",
                "route_short_name",
                "a route needs a short name, a long name, or both",
            ));
        }
        "calendar.txt" => {
            if let (Some(a), Some(b)) = (text(row, "start_date"), text(row, "end_date")) {
                if b < a {
                    out.push(err(
                        "dates_backwards",
                        "end_date",
                        format!("end_date {b} is before start_date {a}"),
                    ));
                }
            }
        }
        "feed_info.txt" => {
            if let (Some(a), Some(b)) = (text(row, "feed_start_date"), text(row, "feed_end_date")) {
                if b < a {
                    out.push(err(
                        "dates_backwards",
                        "feed_end_date",
                        format!("feed_end_date {b} is before feed_start_date {a}"),
                    ));
                }
            }
        }
        "timeframes.txt" => {
            together(row, "start_time", "end_time", &mut out);
            if let (Some(a), Some(b)) = (int(row, "start_time"), int(row, "end_time")) {
                if b <= a {
                    out.push(err(
                        "times_backwards",
                        "end_time",
                        "a timeframe ends after it starts",
                    ));
                }
                if b > 24 * 3600 {
                    out.push(err(
                        "invalid_time",
                        "end_time",
                        "a timeframe ends by 24:00:00",
                    ));
                }
            }
        }
        "fare_leg_join_rules.txt" => together(row, "from_stop_id", "to_stop_id", &mut out),
        "fare_transfer_rules.txt" => {
            if has(row, "duration_limit") != has(row, "duration_limit_type") {
                out.push(err(
                    "fields_go_together",
                    "duration_limit_type",
                    "duration_limit_type is given exactly when duration_limit is",
                ));
            }
            let same_group = matches!(
                (text(row, "from_leg_group_id"), text(row, "to_leg_group_id")),
                (Some(a), Some(b)) if a == b
            );
            if same_group && !has(row, "transfer_count") {
                out.push(err(
                    "missing_field",
                    "transfer_count",
                    "a transfer within one leg group says how many transfers it allows (transfer_count, -1 for no limit)",
                ));
            }
            if !same_group && has(row, "transfer_count") {
                out.push(err(
                    "field_forbidden",
                    "transfer_count",
                    "transfer_count is only for a transfer within one leg group",
                ));
            }
        }
        "transfers.txt" => {
            let kind = int(row, "transfer_type").unwrap_or(0);
            if (1..=3).contains(&kind) {
                for f in ["from_stop_id", "to_stop_id"] {
                    if !has(row, f) {
                        out.push(err(
                            "missing_field",
                            f,
                            format!("a transfer of type {kind} names both stops"),
                        ));
                    }
                }
            }
            if (4..=5).contains(&kind) {
                for f in ["from_trip_id", "to_trip_id"] {
                    if !has(row, f) {
                        out.push(err(
                            "missing_field",
                            f,
                            format!("an in-seat transfer (type {kind}) names both trips"),
                        ));
                    }
                }
            }
            if has(row, "min_transfer_time") && kind != 2 {
                out.push(RuleFinding {
                    error: false,
                    code: "min_transfer_time_unused",
                    field: "min_transfer_time",
                    message: "min_transfer_time means something only on a transfer of type 2"
                        .into(),
                });
            }
        }
        "pathways.txt" => {
            if text(row, "from_stop_id").is_some()
                && text(row, "from_stop_id") == text(row, "to_stop_id")
            {
                out.push(err(
                    "pathway_to_itself",
                    "to_stop_id",
                    "a pathway leads from one location to another",
                ));
            }
            if int(row, "pathway_mode") == Some(6) && int(row, "is_bidirectional") == Some(1) {
                out.push(err(
                    "fare_gate_both_ways",
                    "is_bidirectional",
                    "a fare gate (pathway_mode 6) goes one way",
                ));
            }
        }
        "booking_rules.txt" => {
            let kind = int(row, "booking_type").unwrap_or(0);
            let prior = [
                "prior_notice_duration_min",
                "prior_notice_duration_max",
                "prior_notice_last_day",
                "prior_notice_last_time",
                "prior_notice_start_day",
                "prior_notice_start_time",
                "prior_notice_service_id",
            ];
            match kind {
                0 => {
                    for f in prior {
                        if has(row, f) {
                            out.push(err(
                                "field_forbidden",
                                f,
                                "a real-time booking rule has no prior notice",
                            ));
                        }
                    }
                }
                1 => {
                    if !has(row, "prior_notice_duration_min") {
                        out.push(err(
                            "missing_field",
                            "prior_notice_duration_min",
                            "a same-day booking rule says how long before (prior_notice_duration_min)",
                        ));
                    }
                    for f in [
                        "prior_notice_last_day",
                        "prior_notice_last_time",
                        "prior_notice_service_id",
                    ] {
                        if has(row, f) {
                            out.push(err(
                                "field_forbidden",
                                f,
                                "a same-day booking rule counts minutes, not days",
                            ));
                        }
                    }
                }
                _ => {
                    if !has(row, "prior_notice_last_day") {
                        out.push(err(
                            "missing_field",
                            "prior_notice_last_day",
                            "a booking rule a day or more ahead says which day (prior_notice_last_day)",
                        ));
                    }
                    for f in ["prior_notice_duration_min", "prior_notice_duration_max"] {
                        if has(row, f) {
                            out.push(err(
                                "field_forbidden",
                                f,
                                "a booking rule a day or more ahead counts days, not minutes",
                            ));
                        }
                    }
                }
            }
            together(
                row,
                "prior_notice_last_day",
                "prior_notice_last_time",
                &mut out,
            );
            together(
                row,
                "prior_notice_start_day",
                "prior_notice_start_time",
                &mut out,
            );
        }
        "translations.txt" => {
            let table = text(row, "table_name").unwrap_or("");
            const TABLES: [&str; 9] = [
                "agency",
                "stops",
                "routes",
                "trips",
                "stop_times",
                "pathways",
                "levels",
                "feed_info",
                "attributions",
            ];
            if !TABLES.contains(&table) {
                out.push(err(
                    "invalid_value",
                    "table_name",
                    format!("table_name is one of {}", TABLES.join(", ")),
                ));
            }
            if table == "feed_info" {
                for f in ["record_id", "record_sub_id", "field_value"] {
                    if has(row, f) {
                        out.push(err(
                            "field_forbidden",
                            f,
                            "a feed_info translation names no row",
                        ));
                    }
                }
            } else {
                if has(row, "record_id") == has(row, "field_value") {
                    out.push(err(
                        "missing_field",
                        "record_id",
                        "a translation names its row by record_id or by field_value, exactly one",
                    ));
                }
                if has(row, "record_sub_id") != (table == "stop_times" && has(row, "record_id")) {
                    out.push(err(
                        "invalid_value",
                        "record_sub_id",
                        "record_sub_id is given exactly for a stop_times translation named by record_id",
                    ));
                }
            }
        }
        "attributions.txt" => {
            let roles = ["is_producer", "is_operator", "is_authority"];
            if !roles.iter().any(|r| int(row, r) == Some(1)) {
                out.push(err(
                    "missing_field",
                    "is_producer",
                    "an attribution is at least one of producer, operator and authority",
                ));
            }
            let named = ["agency_id", "route_id", "trip_id"]
                .iter()
                .filter(|f| has(row, f))
                .count();
            if named > 1 {
                out.push(err(
                    "field_forbidden",
                    "route_id",
                    "an attribution names one agency, route or trip at most",
                ));
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(pairs: &[(&'static str, Value)]) -> Row {
        pairs.iter().cloned().collect()
    }

    fn codes(file: &str, r: &Row) -> Vec<&'static str> {
        row_findings(file, r).iter().map(|f| f.code).collect()
    }

    #[test]
    fn an_entrance_needs_a_station_and_a_station_needs_nothing_above_it() {
        assert_eq!(
            codes(
                "stops.txt",
                &row(&[("stop_name", json!("Gate A")), ("location_type", json!(2))])
            ),
            vec!["missing_field"]
        );
        assert_eq!(
            codes(
                "stops.txt",
                &row(&[
                    ("stop_name", json!("C")),
                    ("location_type", json!(1)),
                    ("parent_station", json!("X"))
                ])
            ),
            vec!["station_with_parent"]
        );
        // a generic node may go without a name; a platform may not
        assert!(codes(
            "stops.txt",
            &row(&[("location_type", json!(3)), ("parent_station", json!("S"))])
        )
        .is_empty());
        assert_eq!(codes("stops.txt", &row(&[])), vec!["missing_field"]);
    }

    #[test]
    fn a_transfer_names_what_its_type_needs() {
        assert!(codes("transfers.txt", &row(&[("transfer_type", json!(0))])).is_empty());
        assert_eq!(
            codes(
                "transfers.txt",
                &row(&[("transfer_type", json!(2)), ("from_stop_id", json!("A"))])
            ),
            vec!["missing_field"]
        );
        assert_eq!(
            codes("transfers.txt", &row(&[("transfer_type", json!(4))])),
            vec!["missing_field", "missing_field"]
        );
        let f = row_findings(
            "transfers.txt",
            &row(&[
                ("transfer_type", json!(0)),
                ("min_transfer_time", json!(60)),
            ]),
        );
        assert_eq!((f[0].code, f[0].error), ("min_transfer_time_unused", false));
    }

    #[test]
    fn fare_transfer_counts_only_within_a_group() {
        let same = row(&[
            ("from_leg_group_id", json!("G")),
            ("to_leg_group_id", json!("G")),
            ("fare_transfer_type", json!(0)),
        ]);
        assert_eq!(
            codes("fare_transfer_rules.txt", &same),
            vec!["missing_field"]
        );
        let mut other = same.clone();
        other.insert("to_leg_group_id", json!("H"));
        other.insert("transfer_count", json!(1));
        assert_eq!(
            codes("fare_transfer_rules.txt", &other),
            vec!["field_forbidden"]
        );
        let mut limited = same.clone();
        limited.insert("transfer_count", json!(-1));
        limited.insert("duration_limit", json!(5400));
        assert_eq!(
            codes("fare_transfer_rules.txt", &limited),
            vec!["fields_go_together"]
        );
    }

    #[test]
    fn booking_rules_follow_their_type() {
        assert_eq!(
            codes(
                "booking_rules.txt",
                &row(&[
                    ("booking_type", json!(0)),
                    ("prior_notice_duration_min", json!(30))
                ])
            ),
            vec!["field_forbidden"]
        );
        assert_eq!(
            codes("booking_rules.txt", &row(&[("booking_type", json!(1))])),
            vec!["missing_field"]
        );
        assert_eq!(
            codes(
                "booking_rules.txt",
                &row(&[
                    ("booking_type", json!(2)),
                    ("prior_notice_last_day", json!(1))
                ])
            ),
            vec!["fields_go_together"]
        );
    }

    #[test]
    fn a_translation_names_its_row_one_way() {
        let base = [
            ("table_name", json!("stops")),
            ("field_name", json!("stop_name")),
            ("language", json!("ta")),
            ("translation", json!("x")),
        ];
        assert_eq!(
            codes("translations.txt", &row(&base)),
            vec!["missing_field"]
        );
        let mut by_id = row(&base);
        by_id.insert("record_id", json!("S1"));
        assert!(codes("translations.txt", &by_id).is_empty());
        let mut feed = row(&base);
        feed.insert("table_name", json!("feed_info"));
        feed.insert("record_id", json!("S1"));
        assert_eq!(codes("translations.txt", &feed), vec!["field_forbidden"]);
        let mut bad = row(&base);
        bad.insert("table_name", json!("shapes"));
        bad.insert("field_value", json!("x"));
        assert_eq!(codes("translations.txt", &bad), vec!["invalid_value"]);
    }

    #[test]
    fn times_and_dates_run_forwards() {
        assert_eq!(
            codes(
                "timeframes.txt",
                &row(&[("start_time", json!(36000)), ("end_time", json!(3600))])
            ),
            vec!["times_backwards"]
        );
        assert_eq!(
            codes("timeframes.txt", &row(&[("start_time", json!(36000))])),
            vec!["fields_go_together"]
        );
        assert_eq!(
            codes(
                "calendar.txt",
                &row(&[
                    ("start_date", json!("2026-02-01")),
                    ("end_date", json!("2026-01-01"))
                ])
            ),
            vec!["dates_backwards"]
        );
    }

    #[test]
    fn an_attribution_has_a_role_and_names_one_thing() {
        assert_eq!(
            codes(
                "attributions.txt",
                &row(&[("organization_name", json!("X"))])
            ),
            vec!["missing_field"]
        );
        assert_eq!(
            codes(
                "attributions.txt",
                &row(&[
                    ("is_operator", json!(1)),
                    ("route_id", json!("R")),
                    ("trip_id", json!("T"))
                ])
            ),
            vec!["field_forbidden"]
        );
    }
}
