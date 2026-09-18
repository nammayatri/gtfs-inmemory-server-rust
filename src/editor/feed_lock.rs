//! One lock per feed for every transaction that replays or applies a draft, and
//! the retry that backs it (docs/gtfs-editor.md section 3, "Commit").
//!
//! A draft is validated by replaying it: real UPDATEs on the live tables, in
//! savepoints, rolled back at the end. Two of those at once on one feed - a
//! commit and the set detail a script's add returns, say - take row locks in
//! whatever order their changes come in, and Postgres resolves the collision by
//! killing one with `deadlock detected`, which used to come back as the change's
//! own `database_rejected` finding. So every such transaction first takes the
//! feed's advisory transaction lock: the second waits for the first to finish
//! and there is nothing left to collide. Reads that do not replay never take it.
//!
//! Should a transaction still hit a serialization failure (SQLSTATE 40P01 or
//! 40001), it is not the change's fault: the error propagates as 503
//! `try_again`, and the transaction is retried a few times before that reaches
//! the caller.

use super::error::{EditorError, EditorResult};
use actix_web::http::StatusCode;
use sqlx::{PgConnection, Row};
use std::future::Future;
use std::time::Duration;

/// The error code every transient refusal answers with.
pub const TRY_AGAIN: &str = "try_again";

/// Backoff before each retry of a transaction that hit a serialization failure.
pub const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(400),
];

/// Take the feed's advisory lock for the rest of the current transaction. Call
/// it before the first row lock the transaction takes, so the transaction never
/// holds a row while it waits here.
pub async fn lock_feed(conn: &mut PgConnection, gtfs_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('gtfs_editor:' || $1))")
        .bind(gtfs_id)
        .execute(conn)
        .await?;
    Ok(())
}

/// [`lock_feed`] for the feed of change set `id`, which is read without a lock
/// first; 404 `change_set_not_found` if there is no such set. The set's own row
/// is locked by the caller afterwards, so a commit - which holds the feed lock
/// and then locks its set - and an edit of another set never wait on each other.
pub async fn lock_feed_of_set(conn: &mut PgConnection, id: uuid::Uuid) -> EditorResult<String> {
    let gtfs_id: String =
        sqlx::query("SELECT gtfs_id FROM gtfs_change_set WHERE change_set_id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?
            .ok_or_else(|| EditorError::not_found("change_set_not_found", "no such change set"))?
            .try_get("gtfs_id")?;
    lock_feed(conn, &gtfs_id).await?;
    Ok(gtfs_id)
}

/// Is this a serialization failure - a deadlock (40P01) or a serialization
/// failure proper (40001)? Neither says anything about the statement that hit
/// it; the whole transaction is retried instead.
pub fn is_transient(e: &sqlx::Error) -> bool {
    matches!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("40P01") | Some("40001")
    )
}

/// The error a transient failure answers with: 503 `try_again`.
pub fn try_again(e: &sqlx::Error) -> EditorError {
    tracing::warn!(tag = "[GTFS EDITOR DB]", error = %e, "serialization failure; the request is retried");
    EditorError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        TRY_AGAIN,
        "another change to this feed was being applied at the same time; please try again",
    )
}

/// Run one request transaction, again after a short backoff each time it ends
/// in 503 `try_again`, up to `RETRY_BACKOFF.len()` retries. `run` must start a
/// fresh transaction each time it is called and leave nothing behind when it
/// fails, which is what every editor transaction does: a failure rolls it back
/// whole.
pub async fn retry_transient<T, F, Fut>(mut run: F) -> EditorResult<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = EditorResult<T>>,
{
    let mut attempt = 0;
    loop {
        match run().await {
            Err(e) if e.code == TRY_AGAIN && attempt < RETRY_BACKOFF.len() => {
                tracing::warn!(
                    tag = "[GTFS EDITOR DB]",
                    attempt = attempt + 1,
                    "retrying the transaction after a serialization failure"
                );
                tokio::time::sleep(RETRY_BACKOFF[attempt]).await;
                attempt += 1;
            }
            done => return done,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::borrow::Cow;

    /// A database error with a given SQLSTATE, as the Postgres driver would
    /// report it.
    #[derive(Debug)]
    pub(crate) struct FakeDbError(pub &'static str, pub &'static str);

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.1)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl sqlx::error::DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            self.1
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    pub(crate) fn db_error(code: &'static str, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError(code, message)))
    }

    #[test]
    fn deadlocks_and_serialization_failures_are_transient() {
        assert!(is_transient(&db_error("40P01", "deadlock detected")));
        assert!(is_transient(&db_error(
            "40001",
            "could not serialize access due to concurrent update"
        )));
        assert!(!is_transient(&db_error("23503", "foreign key violation")));
        assert!(!is_transient(&db_error("23505", "unique violation")));
        assert!(!is_transient(&sqlx::Error::RowNotFound));
    }

    #[test]
    fn a_transient_failure_is_503_try_again_never_a_database_rejection() {
        let e: EditorError = db_error("40P01", "deadlock detected").into();
        assert_eq!((e.status.as_u16(), e.code), (503, TRY_AGAIN));
        assert!(!e.message.contains("deadlock"), "{}", e.message);
        // a genuine refusal stays what it was
        let e: EditorError = db_error("23505", "duplicate key").into();
        assert_eq!((e.status.as_u16(), e.code), (500, "internal"));
    }

    #[actix_web::test]
    async fn the_transaction_is_retried_then_given_up() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let out = retry_transient(|| {
            calls.set(calls.get() + 1);
            let n = calls.get();
            async move {
                if n < 3 {
                    Err(try_again(&db_error("40P01", "deadlock detected")))
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(out.map_err(|e| e.code), Ok(3));

        let calls = Cell::new(0);
        let out: EditorResult<()> = retry_transient(|| {
            calls.set(calls.get() + 1);
            async { Err(try_again(&db_error("40001", "could not serialize"))) }
        })
        .await;
        assert_eq!(out.map_err(|e| e.code), Err(TRY_AGAIN));
        assert_eq!(calls.get(), 1 + RETRY_BACKOFF.len());

        // any other error comes straight back
        let calls = Cell::new(0);
        let out: EditorResult<()> = retry_transient(|| {
            calls.set(calls.get() + 1);
            async { Err(EditorError::conflict("change_set_not_draft", "no")) }
        })
        .await;
        assert_eq!(out.map_err(|e| e.code), Err("change_set_not_draft"));
        assert_eq!(calls.get(), 1);
    }
}
