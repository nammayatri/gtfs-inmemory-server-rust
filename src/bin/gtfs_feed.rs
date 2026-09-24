//! `gtfs_feed`: a feed's GTFS in and out of the editor's tables, from the
//! command line (docs/gtfs-editor.md section 18).
//!
//! ```text
//! gtfs_feed roundtrip --zip Z [--gtfs-id G] [--show N]
//!     read Z, build the model the tables would hold, write it back out and
//!     compare: what an import would keep, what it would drop, and whether
//!     the export gives the feed back. Touches no database.
//! gtfs_feed import --db URL --zip Z [--gtfs-id G] [--seed]
//!     seed feed G (default: the feed_id Z names), which must have no rows
//!     yet, from Z: written, read back, compared with Z in one transaction,
//!     committed only with --seed and when nothing differs. Without --seed it
//!     is a dry run that writes nothing.
//! gtfs_feed draft-import --db URL --zip Z --as EMAIL [--gtfs-id G] [--files F,F] [--write]
//!     bring Z into feed G, which already has rows, through change sets
//!     drafted by EMAIL: its records and calendars first, its trips once
//!     those are committed (run it again). Stops, routes and the feed's stop
//!     orders are compared, never written. Without --write it is a dry run.
//! gtfs_feed export --db URL --gtfs-id G --out FILE
//!     the feed's GTFS zip, from the tables.
//! gtfs_feed validate (--zip Z | --db URL --gtfs-id G) [--today YYYY-MM-DD] [--show N]
//!     the feed report: what the zip, or the feed as its tables hold it,
//!     breaks of the GTFS reference, counted by kind.
//! gtfs_feed compare --a Z --b Z [--files F,F] [--show N]
//!     two zips compared keyed, as the round trip compares: what differs,
//!     file by file and field by field.
//! ```
//!
//! `--db` is never read from the environment: say which database, every time.

use gtfs_routes_service::editor::{draft_import, feed_io};
use gtfs_routes_service::gtfs::{compare, model, read, validate, write, Level};
use std::process::ExitCode;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: gtfs_feed roundtrip --zip Z [--gtfs-id G] [--show N]\n       \
         gtfs_feed import --db URL --zip Z [--gtfs-id G] [--seed]\n       \
         gtfs_feed draft-import --db URL --zip Z --as EMAIL [--gtfs-id G] [--files F,F] [--write]\n       \
         gtfs_feed export --db URL --gtfs-id G --out FILE\n       \
         gtfs_feed validate (--zip Z | --db URL --gtfs-id G) [--today D] [--show N]\n       \
         gtfs_feed compare --a Z --b Z [--files F,F] [--show N]"
    );
    ExitCode::from(2)
}

/// The database, as the person running this named it; its host is echoed so
/// nobody seeds the wrong one by accident.
async fn pool(args: &[String]) -> Result<sqlx::PgPool, ExitCode> {
    let Some(url) = arg(args, "--db") else {
        return Err(usage());
    };
    let host = url
        .split('@')
        .nth(1)
        .and_then(|h| h.split('/').next())
        .unwrap_or("?");
    eprintln!("database: {host}");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|e| {
            eprintln!("cannot connect: {e}");
            ExitCode::from(1)
        })
}

async fn import(args: &[String]) -> ExitCode {
    let Some(zip) = arg(args, "--zip") else {
        return usage();
    };
    let pool = match pool(args).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    let bytes = match std::fs::read(&zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{zip}: {e}");
            return ExitCode::from(1);
        }
    };
    let seed = args.iter().any(|a| a == "--seed");
    let who = feed_io::Importer {
        user_id: None,
        email: None,
        label: "gtfs_feed import".into(),
    };
    let started = std::time::Instant::now();
    match feed_io::import_zip(
        &pool,
        &bytes,
        arg(args, "--gtfs-id").as_deref(),
        !seed,
        &who,
    )
    .await
    {
        Ok(report) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "gtfs_id": report.gtfs_id,
                    "dry_run": report.dry_run,
                    "seeded": report.seeded,
                    "feed_version": report.feed_version,
                    "counts": report.counts,
                    "errors": report.errors,
                    "warnings": report.warnings,
                    "findings": report.findings,
                    "round_trip": report.round_trip,
                    "round_trip_sample": report.round_trip_sample,
                    "ms": started.elapsed().as_millis() as u64,
                }))
                .unwrap_or_default()
            );
            if report.errors > 0 || !report.round_trip.is_empty() || (seed && !report.seeded) {
                ExitCode::from(4)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("{}: {}", e.code, e.message);
            ExitCode::from(1)
        }
    }
}

async fn draft_import(args: &[String]) -> ExitCode {
    let (Some(zip), Some(email)) = (arg(args, "--zip"), arg(args, "--as")) else {
        return usage();
    };
    let pool = match pool(args).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    let bytes = match std::fs::read(&zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{zip}: {e}");
            return ExitCode::from(1);
        }
    };
    let user: Option<uuid::Uuid> = match sqlx::query_scalar(
        "SELECT user_id FROM gtfs_editor_user WHERE lower(email) = lower($1) AND status = 'active'",
    )
    .bind(&email)
    .fetch_optional(&pool)
    .await
    {
        Ok(u) => u,
        Err(e) => {
            eprintln!("cannot read the editor's users: {e}");
            return ExitCode::from(1);
        }
    };
    let Some(user_id) = user else {
        eprintln!(
            "{email} is not an active editor user; the change sets need someone to draft them"
        );
        return ExitCode::from(1);
    };
    let opts = draft_import::DraftImport {
        files: arg(args, "--files").map(|f| f.split(',').map(str::to_string).collect()),
        dry_run: !args.iter().any(|a| a == "--write"),
        user_id,
        email,
    };
    let started = std::time::Instant::now();
    match draft_import::draft_import(&pool, &bytes, arg(args, "--gtfs-id").as_deref(), &opts).await
    {
        Ok(report) => {
            let mut out = serde_json::to_value(&report).unwrap_or_default();
            out["ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
            println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
            if report.errors > 0 {
                ExitCode::from(4)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(e) => {
            eprintln!("{}: {}", e.code, e.message);
            ExitCode::from(1)
        }
    }
}

async fn export(args: &[String]) -> ExitCode {
    let (Some(g), Some(out)) = (arg(args, "--gtfs-id"), arg(args, "--out")) else {
        return usage();
    };
    let pool = match pool(args).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    let mut conn = match pool.acquire().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let (m, findings) = match feed_io::load_model(&mut conn, &g).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{}: {}", e.code, e.message);
            return ExitCode::from(1);
        }
    };
    for f in &findings {
        eprintln!("{:?} {} {}", f.level, f.code, f.message);
    }
    let bytes = match write::zip_bytes(&write::to_raw(&m)) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = std::fs::write(&out, &bytes) {
        eprintln!("{out}: {e}");
        return ExitCode::from(1);
    }
    println!("{out}: {} bytes", bytes.len());
    for (file, n) in m.counts() {
        println!("  {file:<24} {n}");
    }
    ExitCode::SUCCESS
}

/// The gtfs_id a zip names in feed_info.txt, as nandi's preprocessor reads it.
fn feed_id_of(raw: &read::RawFeed) -> Option<String> {
    let t = raw.table("feed_info.txt")?;
    let row = t.rows.first()?;
    Some(t.cell(row, "feed_id").trim().to_string()).filter(|s| !s.is_empty())
}

async fn validate(args: &[String]) -> ExitCode {
    let show: usize = arg(args, "--show")
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let today = arg(args, "--today").unwrap_or_else(|| chrono::Utc::now().date_naive().to_string());
    let (m, mut findings, what) = if let Some(zip) = arg(args, "--zip") {
        let read = std::fs::read(&zip)
            .map_err(|e| e.to_string())
            .and_then(|bytes| read::read_zip(&bytes));
        let (raw, mut findings) = match read {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{zip}: {e}");
                return ExitCode::from(1);
            }
        };
        let gtfs_id = arg(args, "--gtfs-id")
            .or_else(|| feed_id_of(&raw))
            .unwrap_or_else(|| "feed".into());
        let (m, more) = model::FeedModel::from_raw(&raw, &gtfs_id, model::BuildOptions::default());
        findings.extend(more);
        (m, findings, zip)
    } else {
        let Some(g) = arg(args, "--gtfs-id") else {
            return usage();
        };
        let pool = match pool(args).await {
            Ok(p) => p,
            Err(code) => return code,
        };
        let mut conn = match pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cannot connect: {e}");
                return ExitCode::from(1);
            }
        };
        match feed_io::load_model(&mut conn, &g).await {
            Ok((m, findings)) => (m, findings, format!("feed {g}")),
            Err(e) => {
                eprintln!("{}: {}", e.code, e.message);
                return ExitCode::from(1);
            }
        }
    };
    findings.extend(validate::validate(&m, &today));
    let report = validate::summarise(&findings, show);
    println!(
        "{what}: {} errors, {} warnings (today {today})",
        report.errors, report.warnings
    );
    for c in &report.codes {
        println!(
            "  {:<7} {:<26} {:<22} {}",
            format!("{:?}", c.level).to_lowercase(),
            c.code,
            c.file.as_deref().unwrap_or(""),
            c.count
        );
        for s in &c.samples {
            println!("          {s}");
        }
    }
    if report.errors > 0 {
        ExitCode::from(4)
    } else {
        ExitCode::SUCCESS
    }
}

fn compare_zips(args: &[String]) -> ExitCode {
    let (Some(a), Some(b)) = (arg(args, "--a"), arg(args, "--b")) else {
        return usage();
    };
    let show: usize = arg(args, "--show")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let only: Option<Vec<String>> =
        arg(args, "--files").map(|f| f.split(',').map(str::to_string).collect());
    let mut feeds = Vec::new();
    for zip in [&a, &b] {
        let raw = std::fs::read(zip)
            .map_err(|e| e.to_string())
            .and_then(|bytes| read::read_zip(&bytes));
        match raw {
            Ok((mut raw, _)) => {
                if let Some(only) = &only {
                    raw.files.retain(|name, _| only.contains(name));
                }
                feeds.push(raw);
            }
            Err(e) => {
                eprintln!("{zip}: {e}");
                return ExitCode::from(1);
            }
        }
    }
    let diffs = compare::compare(&feeds[0], &feeds[1], &Default::default());
    println!("{a} vs {b}: {} differences", diffs.len());
    for (what, n) in compare::summarise(&diffs) {
        println!("    {what}: {n}");
    }
    for d in diffs.iter().take(show) {
        println!(
            "    {} [{}] {}: {:?} -> {:?}",
            d.file,
            d.key.replace('\u{1f}', "|"),
            d.field.as_deref().unwrap_or("(row)"),
            d.a,
            d.b
        );
    }
    if diffs.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
    }
}

fn roundtrip(args: &[String]) -> ExitCode {
    let Some(zip) = arg(args, "--zip") else {
        return usage();
    };
    let show: usize = arg(args, "--show")
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let bytes = match std::fs::read(&zip) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{zip}: {e}");
            return ExitCode::from(1);
        }
    };
    let started = std::time::Instant::now();
    let (raw, mut findings) = match read::read_zip(&bytes) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{zip}: {e}");
            return ExitCode::from(1);
        }
    };
    let gtfs_id = arg(args, "--gtfs-id")
        .or_else(|| feed_id_of(&raw))
        .unwrap_or_else(|| "feed".into());
    let (m, more) = model::FeedModel::from_raw(&raw, &gtfs_id, model::BuildOptions::default());
    findings.extend(more);
    let out = write::to_raw(&m);
    let again = write::zip_bytes(&out).expect("a zip");
    let (back, _) = read::read_zip(&again).expect("our own zip reads");
    let diffs = compare::compare(&raw, &back, &m.dropped);
    let took = started.elapsed();

    println!("{zip} -> {gtfs_id} ({} ms)", took.as_millis());
    for (file, n) in m.counts() {
        println!("  {file:<24} {n}");
    }
    let errors = findings.iter().filter(|f| f.level == Level::Error).count();
    println!("  findings: {} ({errors} errors)", findings.len());
    for f in findings.iter().take(show.max(1) * 3) {
        println!(
            "    {:?} {} {}{}: {}",
            f.level,
            f.code,
            f.file.as_deref().unwrap_or(""),
            f.line.map(|l| format!(":{l}")).unwrap_or_default(),
            f.message
        );
    }
    println!("  round trip: {} differences", diffs.len());
    for (what, n) in compare::summarise(&diffs) {
        println!("    {what}: {n}");
    }
    for d in diffs.iter().take(show) {
        println!(
            "    {} [{}] {}: {:?} -> {:?}",
            d.file,
            d.key.replace('\u{1f}', "|"),
            d.field.as_deref().unwrap_or("(row)"),
            d.a,
            d.b
        );
    }
    if errors == 0 && diffs.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(4)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("roundtrip") => roundtrip(&args[1..]),
        Some("import") => import(&args[1..]).await,
        Some("draft-import") => draft_import(&args[1..]).await,
        Some("export") => export(&args[1..]).await,
        Some("compare") => compare_zips(&args[1..]),
        Some("validate") => validate(&args[1..]).await,
        _ => usage(),
    }
}
