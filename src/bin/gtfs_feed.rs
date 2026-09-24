//! `gtfs_feed`: a feed's GTFS in and out of the editor's tables, from the
//! command line (docs/gtfs-editor.md section 18).
//!
//! ```text
//! gtfs_feed roundtrip --zip Z [--gtfs-id G] [--show N]
//!     read Z, build the model the tables would hold, write it back out and
//!     compare: what an import would keep, what it would drop, and whether
//!     the export gives the feed back. Touches no database.
//! ```

use gtfs_routes_service::gtfs::{compare, model, read, write, Level};
use std::process::ExitCode;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn usage() -> ExitCode {
    eprintln!("usage: gtfs_feed roundtrip --zip Z [--gtfs-id G] [--show N]");
    ExitCode::from(2)
}

/// The gtfs_id a zip names in feed_info.txt, as nandi's preprocessor reads it.
fn feed_id_of(raw: &read::RawFeed) -> Option<String> {
    let t = raw.table("feed_info.txt")?;
    let row = t.rows.first()?;
    Some(t.cell(row, "feed_id").trim().to_string()).filter(|s| !s.is_empty())
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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("roundtrip") => roundtrip(&args[1..]),
        _ => usage(),
    }
}
