//! A deliberately small, deliberately read-only ClickHouse client.
//!
//! It exists for one caller - the GTFS editor's "map line from GPS"
//! (docs/gtfs-editor.md section 17) - which reads bus pings out of a
//! PRODUCTION cluster that other teams share, with a user that has WRITE
//! rights on it. Nothing here has any business writing, so read-only is
//! enforced three times over rather than trusted to good intentions, the same
//! way nandi's `scripts/chennai-bus/src/cleanup/ch_client.py` does:
//!
//!  1. `readonly=2` is sent as a setting on every request. ClickHouse then
//!     refuses INSERT/ALTER/DROP/CREATE server-side - the strongest guard,
//!     because it does not depend on this file being right. (2 rather than 1:
//!     1 also forbids changing settings, which would block the
//!     `max_execution_time` ceiling below.)
//!  2. Every statement is checked here before it is sent: it must open with
//!     SELECT or WITH, carry a LIMIT, name no write keyword, and call none of
//!     the table functions that reach outside the cluster (`url()`, `s3()`,
//!     `remote()`...) - readonly=2, unlike 1, still allows those.
//!  3. A statement holding a `;` anywhere is refused, so nothing can be
//!     smuggled in behind one.
//!
//! It is also a shared cluster, so the client is unhurried on purpose: one
//! query at a time per reader (a pod has one), a minimum gap between queries,
//! a server-side time ceiling on each (`max_execution_time`, so a runaway
//! query is killed at the server rather than merely abandoned by us), two
//! threads and a low priority.
//!
//! Transport notes learned against this deployment and encoded here: JSON
//! output formats stall and never return, so answers come back as
//! `TabSeparated`; and some network paths stall on answers above ~300 rows,
//! so callers page (or pack many values into one row) and keep pages small.
//!
//! Credentials: the password is sent as HTTP basic auth and appears in no
//! URL, log line, error or `Debug` output. A transport error is reported
//! without its URL.

use once_cell::sync::Lazy;
use regex::Regex;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// The politeness floor between two queries against the shared cluster.
pub const DEFAULT_MIN_GAP: Duration = Duration::from_millis(350);

#[derive(Clone)]
pub struct ClickHouseSettings {
    /// The HTTP(S) interface, e.g. `https://ch.example:8443` (not the native
    /// TCP port 9000/9440).
    pub url: String,
    pub user: String,
    pub password: Option<String>,
    /// Server-side ceiling on one query (`max_execution_time`); the HTTP
    /// request itself gives up a few seconds after it.
    pub query_timeout: Duration,
    pub min_gap: Duration,
}

impl std::fmt::Debug for ClickHouseSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseSettings")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("query_timeout", &self.query_timeout)
            .field("min_gap", &self.min_gap)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClickHouseError {
    /// Refused here, before anything was sent. Always a bug in the caller.
    #[error("refused before sending: {0}")]
    ReadOnly(String),
    #[error("ClickHouse did not answer within {0} s")]
    Timeout(u64),
    #[error("ClickHouse could not be reached: {0}")]
    Unreachable(String),
    #[error("ClickHouse answered HTTP {status}: {body}")]
    Http { status: u16, body: String },
}

/// Opening keywords a statement may start with.
const ALLOWED_START: [&str; 2] = ["SELECT", "WITH"];

/// Words that have no place in a read. Scanned over the statement with its
/// string literals blanked, so a route called "UPDATE" cannot trip it.
/// `SETTINGS` and `FORMAT` are here too: this client sets both itself.
static FORBIDDEN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(INSERT|ALTER|DROP|CREATE|TRUNCATE|RENAME|ATTACH|DETACH|OPTIMIZE|GRANT|REVOKE|KILL|DELETE|UPDATE|REPLACE\s+TABLE|EXCHANGE|UNDROP|BACKUP|RESTORE|INTO|OUTFILE|SETTINGS|FORMAT|SET)\b",
    )
    .expect("valid regex")
});

/// `SYSTEM` is a command unless it qualifies a table (`system.columns`).
static SYSTEM_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bSYSTEM\b").expect("valid regex"));

/// Table functions that read outside the cluster, or run things. readonly=2
/// does not forbid them, so this does.
static TABLE_FUNCTIONS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(url|urlCluster|file|fileCluster|remote|remoteSecure|cluster|clusterAllReplicas|s3|s3Cluster|gcs|hdfs|hdfsCluster|azureBlobStorage|mysql|postgresql|mongodb|redis|sqlite|jdbc|odbc|executable|input|dictionary|merge|view|loop)\s*\(",
    )
    .expect("valid regex")
});

static LIMIT: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\bLIMIT\s+\d+").expect("valid regex"));

static STRING_LITERAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"'(?:[^'\\]|\\.)*'").expect("valid regex"));

/// The statement as it will be sent, or why it will not be. Public so the
/// guard can be tested - and used - without a server.
pub fn assert_read_only(sql: &str) -> Result<String, ClickHouseError> {
    let body = sql.trim().trim_end_matches(';').trim();
    let refuse = |why: String| Err(ClickHouseError::ReadOnly(why));
    if body.contains(';') {
        return refuse("multi-statement bodies are not allowed".into());
    }
    let head = body
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !ALLOWED_START.contains(&head.as_str()) {
        return refuse(format!(
            "statement must start with {}; got {head:?}",
            ALLOWED_START.join(" or ")
        ));
    }
    let stripped = STRING_LITERAL.replace_all(body, "''");
    if let Some(m) = FORBIDDEN.find(&stripped) {
        return refuse(format!("forbidden keyword {:?}", m.as_str()));
    }
    for m in SYSTEM_WORD.find_iter(&stripped) {
        if !stripped[m.end()..].trim_start().starts_with('.') {
            return refuse("forbidden keyword \"SYSTEM\"".into());
        }
    }
    if let Some(m) = TABLE_FUNCTIONS.find(&stripped) {
        return refuse(format!(
            "table function {:?} is not allowed",
            m.as_str().trim_end_matches('(').trim()
        ));
    }
    if !LIMIT.is_match(&stripped) {
        return refuse("every statement needs a LIMIT".into());
    }
    Ok(body.to_string())
}

/// Quote a value as a ClickHouse string literal.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// Parse a `TabSeparated` answer: one `Vec` per row, escapes undone, `\N`
/// (NULL) read as an empty string.
pub fn parse_tsv(text: &str) -> Vec<Vec<String>> {
    text.split('\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.split('\t')
                .map(|cell| {
                    if cell == "\\N" {
                        String::new()
                    } else {
                        unescape(cell)
                    }
                })
                .collect()
        })
        .collect()
}

fn unescape(cell: &str) -> String {
    if !cell.contains('\\') {
        return cell.to_string();
    }
    let mut out = String::with_capacity(cell.len());
    let mut chars = cell.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

pub struct ClickHouseReader {
    settings: ClickHouseSettings,
    http: reqwest::Client,
    /// Held for the whole of a query: one at a time, and the time the last
    /// one finished, for the gap.
    gate: Mutex<Option<Instant>>,
    queries: std::sync::atomic::AtomicU64,
}

impl std::fmt::Debug for ClickHouseReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseReader")
            .field("settings", &self.settings)
            .finish()
    }
}

impl ClickHouseReader {
    pub fn new(settings: ClickHouseSettings) -> Result<Self, String> {
        let url = settings.url.trim();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err("the ClickHouse url must be http:// or https://".into());
        }
        if settings.user.trim().is_empty() {
            return Err("the ClickHouse user is empty".into());
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| format!("cannot build the ClickHouse client: {}", e.without_url()))?;
        Ok(Self {
            settings: ClickHouseSettings {
                url: url.trim_end_matches('/').to_string() + "/",
                ..settings
            },
            http,
            gate: Mutex::new(None),
            queries: Default::default(),
        })
    }

    /// How many statements this reader has sent.
    pub fn queries(&self) -> u64 {
        self.queries.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Run one read-only statement and return its rows.
    pub async fn query(&self, sql: &str) -> Result<Vec<Vec<String>>, ClickHouseError> {
        let body = assert_read_only(sql)?;
        let mut last = self.gate.lock().await;
        if let Some(at) = *last {
            let wait = self.settings.min_gap.saturating_sub(at.elapsed());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        }
        let ceiling = self.settings.query_timeout.as_secs().max(1);
        let sent = self
            .http
            .post(&self.settings.url)
            .query(&[
                // the server itself refuses writes for this request, whatever
                // the statement says
                ("readonly", "2"),
                ("max_execution_time", ceiling.to_string().as_str()),
                // our share of a shared cluster stays modest
                ("max_threads", "2"),
                ("priority", "10"),
            ])
            .basic_auth(&self.settings.user, self.settings.password.as_deref())
            .timeout(Duration::from_secs(ceiling + 10))
            .body(format!("{body} FORMAT TabSeparated"))
            .send()
            .await;
        self.queries
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let result = match sent {
            Err(e) => Err(transport_error(e, ceiling)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                match resp.text().await {
                    Err(e) => Err(transport_error(e, ceiling)),
                    Ok(text) if status == 200 => Ok(parse_tsv(&text)),
                    Ok(text) => Err(ClickHouseError::Http {
                        status,
                        body: self.scrub(&text.chars().take(400).collect::<String>()),
                    }),
                }
            }
        };
        *last = Some(Instant::now());
        result
    }

    /// An error body with the credentials taken out: a failed sign-in names
    /// the user it was for.
    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        if let Some(pw) = self.settings.password.as_deref().filter(|p| !p.is_empty()) {
            out = out.replace(pw, "<redacted>");
        }
        let user = self.settings.user.trim();
        if !user.is_empty() {
            out = out.replace(user, "<user>");
        }
        out
    }

    /// Page a statement with `LIMIT page OFFSET n` until a short page or
    /// `max_rows`. `sql` must not carry its own outer LIMIT.
    pub async fn paged(
        &self,
        sql: &str,
        page: usize,
        max_rows: usize,
    ) -> Result<Vec<Vec<String>>, ClickHouseError> {
        let page = page.max(1);
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < max_rows {
            let rows = self
                .query(&format!("{sql} LIMIT {page} OFFSET {offset}"))
                .await?;
            let n = rows.len();
            out.extend(rows);
            if n < page {
                break;
            }
            offset += page;
        }
        Ok(out)
    }
}

fn transport_error(e: reqwest::Error, ceiling: u64) -> ClickHouseError {
    if e.is_timeout() {
        ClickHouseError::Timeout(ceiling + 10)
    } else {
        // never the URL: it names the cluster
        ClickHouseError::Unreachable(e.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn refused(sql: &str) -> String {
        match assert_read_only(sql) {
            Err(ClickHouseError::ReadOnly(why)) => why,
            other => panic!("{sql:?} was not refused: {other:?}"),
        }
    }

    #[test]
    fn only_a_bounded_select_gets_through() {
        assert!(refused("INSERT INTO t VALUES (1)").contains("must start with"));
        assert!(refused("ALTER TABLE t DELETE WHERE 1").contains("must start with"));
        assert!(refused("DESCRIBE t").contains("must start with"));
        assert!(refused("SHOW TABLES").contains("must start with"));
        assert!(refused("/* hi */ SELECT 1 LIMIT 1").contains("must start with"));
        assert!(refused("SELECT 1; DROP TABLE t").contains("multi-statement"));
        assert!(refused("SELECT 1 LIMIT 1; SELECT 2 LIMIT 1").contains("multi-statement"));
        assert!(refused("SELECT ';' LIMIT 1").contains("multi-statement"));
        assert!(refused("SELECT * FROM t").contains("LIMIT"));
        assert!(refused("SELECT 1 LIMIT 1 SETTINGS readonly = 0").contains("SETTINGS"));
        assert!(refused("SELECT 1 LIMIT 1 FORMAT JSON").contains("FORMAT"));
        assert!(refused("SELECT * FROM url('http://x', CSV) LIMIT 1").contains("url"));
        assert!(refused("SELECT * FROM remote('h', db.t) LIMIT 1").contains("remote"));
        assert!(refused("SELECT * FROM s3 ('b', CSV) LIMIT 1").contains("s3"));
        assert!(
            refused("WITH x AS (SELECT 1) SELECT * FROM x LIMIT 1 INTO OUTFILE 'f'")
                .contains("INTO")
        );
        assert!(
            refused("SELECT 1 LIMIT 1 UNION ALL SELECT * FROM system LIMIT 1").contains("SYSTEM")
        );
        assert!(refused("SELECT (DELETE) LIMIT 1").contains("DELETE"));
    }

    #[test]
    fn reads_the_editor_sends_are_allowed() {
        for sql in [
            "SELECT 1 LIMIT 1",
            "select 1 limit 1;",
            "  WITH 3 AS n SELECT n LIMIT 1",
            "SELECT name, type FROM system.columns WHERE table = 'x' LIMIT 100",
            // a literal may say anything
            "SELECT count() FROM t WHERE routeNumber = 'UPDATE DROP url(' LIMIT 1",
            "SELECT a FROM t WHERE r = 'it\\'s' LIMIT 10 OFFSET 20",
            // update/insert as part of an identifier is not the keyword
            "SELECT updated_at, inserted FROM t LIMIT 1",
        ] {
            assert!(assert_read_only(sql).is_ok(), "{sql}");
        }
        assert_eq!(
            assert_read_only("SELECT 1 LIMIT 1;;").unwrap(),
            "SELECT 1 LIMIT 1"
        );
    }

    #[test]
    fn quoting_and_tsv() {
        assert_eq!(quote("21G"), "'21G'");
        assert_eq!(quote("a'b\\c"), "'a\\'b\\\\c'");
        let q = format!(
            "SELECT 1 FROM t WHERE r = {} LIMIT 1",
            quote("x'; DROP TABLE t")
        );
        // the quoted value still cannot open a second statement
        assert!(assert_read_only(&q).is_err());
        let rows = parse_tsv("a\tb\\tc\t\\N\n1\t2\\n3\t4\n");
        assert_eq!(rows, vec![vec!["a", "b\tc", ""], vec!["1", "2\n3", "4"]]);
        assert!(parse_tsv("").is_empty());
    }

    #[test]
    fn debug_never_shows_the_password() {
        let s = ClickHouseSettings {
            url: "https://ch.invalid:8443".into(),
            user: "reader".into(),
            password: Some("hunter2-very-secret".into()),
            query_timeout: Duration::from_secs(5),
            min_gap: DEFAULT_MIN_GAP,
        };
        let reader = ClickHouseReader::new(s.clone()).unwrap();
        for text in [format!("{s:?}"), format!("{reader:?}")] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
    }

    /// A one-shot HTTP server: records the raw request and answers `answer`.
    async fn one_shot(answer: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf).to_string()
        });
        (format!("http://{addr}"), task)
    }

    #[tokio::test]
    async fn every_request_carries_readonly_2_and_its_limits() {
        let (url, server) = one_shot("7\tx\n").await;
        let reader = ClickHouseReader::new(ClickHouseSettings {
            url,
            user: "reader".into(),
            password: Some("pw".into()),
            query_timeout: Duration::from_secs(7),
            min_gap: DEFAULT_MIN_GAP,
        })
        .unwrap();
        let rows = reader.query("SELECT 7, 'x' LIMIT 1").await.unwrap();
        assert_eq!(rows, vec![vec!["7".to_string(), "x".to_string()]]);
        let req = server.await.unwrap();
        let first = req.lines().next().unwrap();
        assert!(first.starts_with("POST /?"), "{first}");
        assert!(first.contains("readonly=2"), "{first}");
        assert!(first.contains("max_execution_time=7"), "{first}");
        assert!(first.contains("max_threads=2"), "{first}");
        assert!(
            !first.contains("pw"),
            "the password is not in the URL: {first}"
        );
        // basic auth, base64("reader:pw")
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: basic cmvhzgvyonb3"),
            "{req}"
        );
        assert!(
            req.ends_with("SELECT 7, 'x' LIMIT 1 FORMAT TabSeparated"),
            "{req}"
        );
        assert_eq!(reader.queries(), 1);

        // a refused statement never leaves the process
        let err = reader.query("DROP TABLE t").await.unwrap_err();
        assert!(matches!(err, ClickHouseError::ReadOnly(_)));
        assert_eq!(reader.queries(), 1);
    }

    #[tokio::test]
    async fn an_unreachable_server_is_named_without_its_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let reader = ClickHouseReader::new(ClickHouseSettings {
            url: format!("http://127.0.0.1:{port}"),
            user: "reader".into(),
            password: Some("pw".into()),
            query_timeout: Duration::from_secs(2),
            min_gap: Duration::ZERO,
        })
        .unwrap();
        match reader.query("SELECT 1 LIMIT 1").await {
            Err(ClickHouseError::Unreachable(why)) => {
                assert!(!why.contains(&port.to_string()), "{why}");
            }
            other => panic!("{other:?}"),
        }
    }
}
