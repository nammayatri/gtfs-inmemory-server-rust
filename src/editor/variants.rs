//! Temporary routes: a route's stage list, more than once (docs section 20).
//!
//! Roadworks close a street and a route runs somewhere else for a while. That
//! is the same route wearing a different list of stages, so a route may hold
//! any number of lists and wear one at a time. Two columns hold all of it:
//!
//! - `gtfs_route_stage.variant_id` says which list a link belongs to; `NULL` is
//!   the route's normal one, which is why no route needed backfilling.
//! - `gtfs_route.active_variant_id` says which list it is wearing; `NULL` is
//!   normal.
//!
//! There is no table of temporary routes: a temporary route *is* its links, so
//! it exists exactly while some `gtfs_route_stage` row carries its id, and it
//! is deleted by deleting those rows. Its id is what people call it.
//!
//! `gtfs_route_stop` is still the flattened list every reader uses, derived
//! from the list the route is wearing. Going back to normal is the same
//! `activate` with no variant, so there is one code path and the diversion
//! stays for the next time that street closes.
use serde_json::{json, Value};
use sqlx::{PgConnection, Row};

use super::service::{
    blocks_apply, check_stops_usable, fail, load_route_rows, write_route_rows, ApplyError,
};
use super::stages::{active_variant, links_hash, mint_stage_id, route_links, write_links};
use super::validation::{
    check_route_rows, flatten_stages, grade_against_live, stage_numbers, stored_stage_rows,
    Finding, RouteRow, StageLink, StageRow,
};

fn links_in(after: &Value) -> Result<Vec<StageLink>, ApplyError> {
    serde_json::from_value(after["stages"].clone())
        .map_err(|e| fail("invalid_payload", format!("stages: {e}")))
}

/// `after.variant_id`, absent or null being the route's normal list.
fn variant_in(after: &Value) -> Option<String> {
    after
        .get("variant_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn route_is_live(conn: &mut PgConnection, g: &str, route_id: &str) -> Result<(), ApplyError> {
    let row = sqlx::query(
        "SELECT deleted FROM gtfs_route WHERE gtfs_id = $1 AND route_id = $2 FOR UPDATE",
    )
    .bind(g)
    .bind(route_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Err(fail("route_not_found", format!("no route {route_id}"))),
        Some(r) if r.try_get::<bool, _>("deleted")? => Err(fail(
            "route_deleted",
            format!("route {route_id} is deleted"),
        )),
        Some(_) => Ok(()),
    }
}

/// A temporary route exists while its links do.
async fn variant_exists(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant_id: &str,
) -> Result<bool, sqlx::Error> {
    Ok(!route_links(conn, g, route_id, Some(variant_id))
        .await?
        .is_empty())
}

/// The rows a list would make, and what is wrong with them. Fare and order
/// problems are graded against what the route serves now, as a stage edit is:
/// a problem the route already had stays a warning.
async fn rows_of(
    conn: &mut PgConnection,
    g: &str,
    links: &[StageLink],
    live: &[RouteRow],
) -> Result<(Vec<RouteRow>, Vec<Finding>), ApplyError> {
    let mut findings = Vec::new();
    let numbers = stage_numbers(links);
    let mut stages = Vec::with_capacity(links.len());
    for (link, number) in links.iter().zip(&numbers) {
        let sk = match super::stages::resolve_link(conn, g, link).await {
            Ok(sk) => sk,
            Err(e) => {
                findings.push(Finding::error(
                    "stage_not_found",
                    link.stage_id.trim(),
                    e.to_string(),
                ));
                continue;
            }
        };
        let id = sk.label();
        match super::stages::load_stage(conn, g, &sk, false).await? {
            None => findings.push(Finding::error(
                "stage_not_found",
                &id,
                format!("no stage {id}"),
            )),
            Some(s) if s.deleted => findings.push(Finding::error(
                "stage_deleted",
                &id,
                format!("stage {id} is deleted"),
            )),
            Some(s) => stages.push((s.name, *number, s.rows)),
        }
    }
    if blocks_apply(&findings) {
        return Ok((Vec::new(), findings));
    }
    let rows = flatten_stages(
        stages
            .iter()
            .map(|(n, no, r)| (*no, n.as_str(), r.as_slice())),
    );
    let ids: Vec<String> = rows
        .iter()
        .filter_map(|r| r.stop_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    findings.extend(check_stops_usable(conn, g, &ids).await?);
    for f in grade_against_live(check_route_rows(&rows), &check_route_rows(live)) {
        findings.push(f);
    }
    Ok((rows, findings))
}

/// A route edited stop by stop has no stage list to go back to, so the first
/// diversion gives it one: its rows become stages under the normal list, cut
/// where a NEW STOP starts a fare stage. Nothing it serves changes.
pub(super) async fn ensure_normal_list(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    actor: &str,
) -> Result<usize, ApplyError> {
    if !route_links(conn, g, route_id, None).await?.is_empty() {
        return Ok(0);
    }
    let live = load_route_rows(conn, g, route_id).await?;
    if live.is_empty() {
        return Err(fail(
            "route_empty",
            format!("route {route_id} has no stops to build stages from"),
        ));
    }
    // cut the list where a NEW STOP starts a stage; rows before the first one
    // are a stage of their own
    let mut runs: Vec<(i32, String, Vec<RouteRow>)> = Vec::new();
    for row in &live {
        let starts = row.stop_type == "NEW STOP";
        if starts || runs.is_empty() {
            runs.push((row.stage_no, row.stage_name.clone(), Vec::new()));
        }
        runs.last_mut()
            .expect("a run was just pushed")
            .2
            .push(row.clone());
    }
    let mut links: Vec<(super::stages::StageKey, i32)> = Vec::with_capacity(runs.len());
    for (stage_no, stage_name, rows) in runs {
        let stage_id = mint_stage_id(conn, g).await?;
        let stored: Vec<StageRow> = stored_stage_rows(
            &stage_id,
            &rows
                .iter()
                .map(|r| StageRow {
                    stop_id: r.stop_id.clone(),
                    stop_type: r.stop_type.clone(),
                    marker_id: r.marker_id.clone(),
                    marker_name: r.marker_name.clone(),
                    marker_lat: r.marker_lat,
                    marker_lon: r.marker_lon,
                    stop_name_override: r.stop_name_override.clone(),
                })
                .collect::<Vec<_>>(),
        );
        sqlx::query(
            "INSERT INTO gtfs_stage (gtfs_id, stage_id, name, description, provenance, updated_by) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)",
        )
        .bind(g)
        .bind(&stage_id)
        .bind(&stage_name)
        .bind(format!("From route {route_id}"))
        .bind(json!({"source": "route", "route_id": route_id}).to_string())
        .bind(actor)
        .execute(&mut *conn)
        .await?;
        let sk = super::stages::StageKey::new(&stage_id, None);
        super::stages::write_stage_rows(conn, g, &sk, &stored).await?;
        links.push((sk, stage_no));
    }
    let made = links.len();
    write_links(conn, g, route_id, None, &links, actor).await?;
    Ok(made)
}

/// `route_variant/create`: a diversion of its own, not yet worn. Its stages are
/// its whole existence, so a create is the first write of its links.
pub(super) async fn variant_create(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    route_is_live(conn, g, route_id).await?;
    let Some(variant_id) = variant_in(after) else {
        return Err(fail(
            "invalid_payload",
            "variant_id is required to add a temporary route",
        ));
    };
    if variant_exists(conn, g, route_id, &variant_id).await? {
        return Err(fail(
            "variant_exists",
            format!("route {route_id} already has a temporary route called {variant_id}"),
        ));
    }
    let links = links_in(after)?;
    if links.is_empty() {
        return Err(fail(
            "invalid_payload",
            format!("temporary route {variant_id} needs at least one stage"),
        ));
    }
    let live = load_route_rows(conn, g, route_id).await?;
    let (_, findings) = rows_of(conn, g, &links, &live).await?;
    if blocks_apply(&findings) {
        return Ok(findings);
    }
    // the route keeps its own stop list; giving it one as stages is what makes
    // going back to normal a derivation like everything else
    let converted = ensure_normal_list(conn, g, route_id, actor).await?;
    write_links(
        conn,
        g,
        route_id,
        Some(&variant_id),
        &numbered(&links),
        actor,
    )
    .await?;

    let mut out = findings;
    if converted > 0 {
        out.push(Finding::warning(
            "route_built_from_stages",
            route_id,
            format!(
                "route {route_id} was edited stop by stop, so its stops became {converted} stages \
                 it can go back to; what it serves does not change"
            ),
        ));
    }
    Ok(out)
}

/// `route_variant/update`: the stages of a diversion, which is all it has.
pub(super) async fn variant_update(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    route_is_live(conn, g, route_id).await?;
    let Some(variant_id) = variant_in(after) else {
        return Err(fail(
            "variant_is_main",
            "the route's normal stop list is changed with route_stages, not as a temporary route",
        ));
    };
    if !variant_exists(conn, g, route_id, &variant_id).await? {
        return Err(fail(
            "variant_not_found",
            format!("route {route_id} has no temporary route called {variant_id}"),
        ));
    }
    let links = links_in(after)?;
    if links.is_empty() {
        return Err(fail(
            "invalid_payload",
            format!("temporary route {variant_id} needs at least one stage"),
        ));
    }
    let live = load_route_rows(conn, g, route_id).await?;
    let (rows, mut findings) = rows_of(conn, g, &links, &live).await?;
    if blocks_apply(&findings) {
        return Ok(findings);
    }
    write_links(
        conn,
        g,
        route_id,
        Some(&variant_id),
        &numbered(&links),
        actor,
    )
    .await?;
    // the rows it serves change only while it is the list being worn
    if active_variant(conn, g, route_id).await?.as_deref() == Some(variant_id.as_str()) {
        findings.extend(write_route_rows(conn, g, route_id, &rows, &live, actor).await?);
    }
    Ok(findings)
}

/// `route_variant/activate`: wear one of the route's lists. No variant is the
/// route's normal one, which is how a diversion ends.
pub(super) async fn variant_activate(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    route_is_live(conn, g, route_id).await?;
    let wanted = variant_in(after);
    let worn = active_variant(conn, g, route_id).await?;
    let mut findings = Vec::new();

    match &wanted {
        None => {
            if route_links(conn, g, route_id, None).await?.is_empty() {
                return Err(fail(
                    "route_has_no_normal_list",
                    format!("route {route_id} has no normal stop list to go back to"),
                ));
            }
        }
        Some(id) => {
            if !variant_exists(conn, g, route_id, id).await? {
                return Err(fail(
                    "variant_not_found",
                    format!("route {route_id} has no temporary route called {id}"),
                ));
            }
        }
    }
    if worn == wanted {
        findings.push(Finding::warning(
            "variant_unchanged",
            route_id,
            match &wanted {
                None => format!("route {route_id} is already running its normal route"),
                Some(id) => format!("route {route_id} is already running {id}"),
            },
        ));
        return Ok(findings);
    }

    let live = load_route_rows(conn, g, route_id).await?;
    let links = route_links(conn, g, route_id, wanted.as_deref()).await?;
    let (rows, found) = rows_of(conn, g, &links, &live).await?;
    findings.extend(found);
    if blocks_apply(&findings) {
        return Ok(findings);
    }
    sqlx::query(
        "UPDATE gtfs_route SET active_variant_id = $3, updated_by = $4 \
         WHERE gtfs_id = $1 AND route_id = $2",
    )
    .bind(g)
    .bind(route_id)
    .bind(wanted.as_deref())
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    findings.extend(write_route_rows(conn, g, route_id, &rows, &live, actor).await?);

    let was = live.iter().filter(|r| r.stop_id.is_some()).count();
    let now = rows.iter().filter(|r| r.stop_id.is_some()).count();
    findings.push(match &wanted {
        Some(id) => Finding::warning(
            "route_diverted",
            route_id,
            format!(
                "route {route_id} runs {id} from now: {now} stops instead of {was}, until someone \
                 puts it back"
            ),
        ),
        None => Finding::warning(
            "route_back_to_normal",
            route_id,
            format!("route {route_id} runs its normal route again: {now} stops instead of {was}"),
        ),
    });
    Ok(findings)
}

/// `route_variant/delete`: drop a diversion the route is not wearing. Deleting
/// its links deletes it; the stages themselves stay, since other routes and
/// lists may use them.
pub(super) async fn variant_delete(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    after: &Value,
    _actor: &str,
) -> Result<Vec<Finding>, ApplyError> {
    route_is_live(conn, g, route_id).await?;
    let Some(variant_id) = variant_in(after) else {
        return Err(fail(
            "variant_is_main",
            format!(
                "route {route_id}'s normal stop list cannot be deleted; a temporary route is \
                 deleted by its own id"
            ),
        ));
    };
    if !variant_exists(conn, g, route_id, &variant_id).await? {
        return Err(fail(
            "variant_not_found",
            format!("route {route_id} has no temporary route called {variant_id}"),
        ));
    }
    if active_variant(conn, g, route_id).await?.as_deref() == Some(variant_id.as_str()) {
        return Err(fail(
            "variant_active",
            format!(
                "route {route_id} is running {variant_id}; put it back on its normal route first"
            ),
        ));
    }
    sqlx::query(
        "DELETE FROM gtfs_route_stage \
         WHERE gtfs_id = $1 AND route_id = $2 AND variant_id = $3",
    )
    .bind(g)
    .bind(route_id)
    .bind(&variant_id)
    .execute(&mut *conn)
    .await?;
    Ok(Vec::new())
}

/// The links of a list, each with the fare stage number it carries.
fn numbered(links: &[StageLink]) -> Vec<(super::stages::StageKey, i32)> {
    links
        .iter()
        .zip(stage_numbers(links))
        .map(|(l, n)| (super::stages::link_key(l), n))
        .collect()
}

/// The hash a change to one list is based on, for the conflict check at commit.
pub(super) async fn live_variant_hash(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
    variant: Option<&str>,
) -> Result<String, sqlx::Error> {
    Ok(links_hash(&route_links(conn, g, route_id, variant).await?))
}

/// A route's lists, for the route page and the diversions list. The temporary
/// routes are the ids its links carry.
pub(super) async fn route_variants(
    conn: &mut PgConnection,
    g: &str,
    route_id: &str,
) -> Result<Value, sqlx::Error> {
    let worn = active_variant(conn, g, route_id).await?;
    let rows = sqlx::query(
        "SELECT variant_id, count(*) AS stage_count, max(updated_at) AS updated_at \
         FROM gtfs_route_stage \
         WHERE gtfs_id = $1 AND route_id = $2 AND variant_id IS NOT NULL \
         GROUP BY variant_id ORDER BY variant_id",
    )
    .bind(g)
    .bind(route_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut variants: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        let id: String = r.try_get("variant_id")?;
        let active = worn.as_deref() == Some(id.as_str());
        let detail = super::stages::route_stages_of(conn, g, route_id, Some(&id))
            .await
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        variants.push(json!({
            "variant_id": id,
            "active": active,
            "stage_count": r.try_get::<i64, _>("stage_count")?,
            "updated_at": r.try_get::<chrono::DateTime<chrono::Utc>, _>("updated_at")?,
            "stages_hash": detail["stages_hash"].clone(),
            "stages": detail["stages"].clone(),
        }));
    }
    let normal = super::stages::route_stages_of(conn, g, route_id, None)
        .await
        .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
    Ok(json!({
        "route_id": route_id,
        "active_variant_id": worn,
        "diverted": worn.is_some(),
        "has_normal_list": !route_links(conn, g, route_id, None).await?.is_empty(),
        "normal_stages_hash": normal["stages_hash"].clone(),
        "normal_stages": normal["stages"].clone(),
        "variants": variants,
    }))
}

/// Every route running a temporary route right now.
pub(super) async fn diverted_routes(
    conn: &mut PgConnection,
    g: &str,
) -> Result<Vec<Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT r.route_id, r.short_name, r.long_name, r.active_variant_id, \
                (SELECT max(rs.updated_at) FROM gtfs_route_stage rs \
                  WHERE rs.gtfs_id = r.gtfs_id AND rs.route_id = r.route_id \
                    AND rs.variant_id = r.active_variant_id) AS since \
         FROM gtfs_route r \
         WHERE r.gtfs_id = $1 AND NOT r.deleted AND r.active_variant_id IS NOT NULL \
         ORDER BY r.route_id",
    )
    .bind(g)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(json!({
                "route_id": r.try_get::<String, _>("route_id")?,
                "short_name": r.try_get::<Option<String>, _>("short_name")?,
                "long_name": r.try_get::<Option<String>, _>("long_name")?,
                "variant_id": r.try_get::<String, _>("active_variant_id")?,
                "since": r.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("since")?,
            }))
        })
        .collect()
}
