//! Imports as background jobs (docs/gtfs-editor.md section 18.15). A GTFS zip
//! import can run longer than the load balancer and Pomerium in front of the
//! editor wait (30 s each), so the dashboard asks for one in the background:
//! the request is answered at once with a job, the import runs on a thread of
//! its own - its own runtime and a small pool of its own, so its CPU-heavy
//! steps never hold up a web worker - and its report is written to
//! `gtfs_import_job`, where the page polls for it on whichever pod it lands.
//!
//! A map line from GPS (section 17.10) is a job of this table too, kind
//! `gps_line`: [`record`] it, run it where its reader lives, [`finish`] it.

use super::error::{EditorError, EditorResult};
use actix_web::http::StatusCode;
use serde_json::{json, Value};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::future::Future;
use std::pin::Pin;
use tracing::error;
use uuid::Uuid;

/// A job still running this long after it started is lost: the pod running it
/// went away. An import is far shorter, and a GPS line gives up sooner.
const LOST_AFTER_MINUTES: i32 = 30;

/// The import itself, given the job's own pool.
pub type Work =
    Box<dyn FnOnce(PgPool) -> Pin<Box<dyn Future<Output = EditorResult<Value>>>> + Send>;

/// Record a running job: its id, for [`finish`] once it is done.
pub async fn record(
    pool: &PgPool,
    gtfs_id: Option<&str>,
    kind: &str,
    dry_run: bool,
    user_id: Uuid,
) -> EditorResult<Uuid> {
    let job_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO gtfs_import_job (job_id, gtfs_id, kind, dry_run, created_by, pod) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(job_id)
    .bind(gtfs_id)
    .bind(kind)
    .bind(dry_run)
    .bind(user_id)
    .bind(std::env::var("HOSTNAME").ok())
    .execute(pool)
    .await?;
    Ok(job_id)
}

/// Record a running job and start `work` on a thread of its own; the job's id,
/// at once.
pub async fn start(
    pool: &PgPool,
    gtfs_id: Option<&str>,
    kind: &str,
    dry_run: bool,
    user_id: Uuid,
    work: Work,
) -> EditorResult<Uuid> {
    let job_id = record(pool, gtfs_id, kind, dry_run, user_id).await?;
    // a pool's connections belong to the runtime that made them: the job's
    // thread opens its own, with the same settings
    let options = pool.connect_options().as_ref().clone();
    let spawned = std::thread::Builder::new()
        .name("gtfs-import-job".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => return error!("import job {job_id}: no runtime: {e}"),
            };
            rt.block_on(async move {
                let pool = match PgPoolOptions::new()
                    .max_connections(2)
                    .connect_with(options)
                    .await
                {
                    Ok(p) => p,
                    Err(e) => return error!("import job {job_id}: cannot connect: {e}"),
                };
                let outcome = work(pool.clone()).await;
                if let Err(e) = finish(&pool, job_id, outcome).await {
                    error!("import job {job_id}: its outcome was not recorded: {e}");
                }
                pool.close().await;
            });
        });
    if let Err(e) = spawned {
        let failed = || {
            EditorError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "job_not_started",
                format!("the import could not be started: {e}"),
            )
        };
        finish(pool, job_id, Err(failed())).await?;
        return Err(failed());
    }
    Ok(job_id)
}

/// Write a job's outcome: its report, or the error it answered, with the
/// error's details (a GPS line's counts say why it found none).
pub async fn finish(
    pool: &PgPool,
    job_id: Uuid,
    outcome: EditorResult<Value>,
) -> Result<(), sqlx::Error> {
    let (status, report, error) = match outcome {
        Ok(report) => ("done", Some(report.to_string()), None),
        Err(e) => {
            let mut error =
                json!({"status": e.status.as_u16(), "code": e.code, "message": e.message});
            if !e.details.is_null() {
                error["details"] = e.details;
            }
            ("failed", None, Some(error.to_string()))
        }
    };
    sqlx::query(
        "UPDATE gtfs_import_job SET status = $2, report = $3::jsonb, error = $4::jsonb, \
         finished_at = now() WHERE job_id = $1",
    )
    .bind(job_id)
    .bind(status)
    .bind(report)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// A job as the page polls it: `running`, `done` with its report, `failed`
/// with the error the import answered, or `lost`.
pub async fn get(pool: &PgPool, job_id: Uuid) -> EditorResult<Value> {
    let row = sqlx::query(
        "SELECT gtfs_id, kind, dry_run, report::text AS report, error::text AS error, \
         created_at, finished_at, \
         CASE WHEN status = 'running' AND created_at < now() - make_interval(mins => $2) \
              THEN 'lost' ELSE status END AS status \
         FROM gtfs_import_job WHERE job_id = $1",
    )
    .bind(job_id)
    .bind(LOST_AFTER_MINUTES)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| EditorError::not_found("job_not_found", format!("no job {job_id}")))?;
    let json_of = |s: Option<String>| {
        s.and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null)
    };
    let status: String = row.try_get("status")?;
    let error = if status == "lost" {
        json!({"status": 500, "code": "job_lost",
               "message": "The job stopped without an answer: the server running it restarted. Try it again."})
    } else {
        json_of(row.try_get("error")?)
    };
    Ok(json!({
        "job_id": job_id,
        "gtfs_id": row.try_get::<Option<String>, _>("gtfs_id")?,
        "kind": row.try_get::<String, _>("kind")?,
        "dry_run": row.try_get::<bool, _>("dry_run")?,
        "status": status,
        "report": json_of(row.try_get("report")?),
        "error": error,
        "created_at": row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")?,
        "finished_at": row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("finished_at")?,
    }))
}
