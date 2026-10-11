//! ETA variants, their per-segment times, and the temporary overrides that swap one in.
//!
//! Split out of `db_vehicle_reader_internal` because it is a self-contained concern with its
//! own catalogue, caches and validation; that module reads vehicles and waybills. Both share
//! `DBVehicleReaderInternal` so a single connection pool and cache set serve the schedule
//! reads that resolve a variant and the writes that set one.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, error, info};

use super::db_vehicle_reader_internal::DBVehicleReaderInternal;
use crate::models::{
    ActiveTripEtaOverride, EtaVariant, StationEtaEntry, StationEtaMap, StationEtaRow,
};
use crate::tools::error::{AppError, AppResult};

/// Does a window overlap the run `w` makes of trip `bstd`? Matched against the trip's own clock,
/// never `now()`: one schedule trip is re-run by a new waybill each duty date.
///
/// `Asia/Kolkata` is load-bearing -- Postgres reads an offset *string* POSIX-style, so
/// `AT TIME ZONE '+05:30'` shifts 11 hours the wrong way.
///
/// Every cast sits behind a fully-bounded regex arm of the same CASE, so a bad row carries no
/// override rather than aborting a schedule read that is mostly override-free.
macro_rules! eta_override_window_overlap_sql {
    ($from:expr, $untill:expr) => {
        concat!(
            r#"COALESCE(
                    CASE
                        WHEN w.duty_date !~ '^\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])$'
                          OR bstd.start_time !~ '^([01]\d|2[0-3]|\d):[0-5]\d$'
                          OR bstd.end_time !~ '^([01]\d|2[0-3]|\d):[0-5]\d$' THEN false
                        ELSE ((w.duty_date || ' ' || bstd.start_time)::timestamp
                                  AT TIME ZONE 'Asia/Kolkata') <= "#,
            $untill,
            r#"
                         AND (((w.duty_date || ' ' || bstd.end_time)::timestamp
                               + CASE WHEN bstd.end_time::time < bstd.start_time::time
                                      THEN interval '1 day' ELSE interval '0' END)
                              AT TIME ZONE 'Asia/Kolkata') >= "#,
            $from,
            r#"
                    END, false)"#
        )
    };
}

/// The stored override, if it applies to this run. A retired variant reads as absent.
macro_rules! eta_override_in_force_sql {
    () => {
        concat!(
            r#"(bstd.eta_override_variant_id IS NOT NULL
                AND EXISTS (SELECT 1 FROM eta_variant v
                            WHERE v.variant_id = bstd.eta_override_variant_id
                              AND NOT v.deleted)
                AND "#,
            eta_override_window_overlap_sql!(
                "bstd.eta_override_effective_from",
                "bstd.eta_override_effective_untill"
            ),
            ")"
        )
    };
}

/// The two override-derived columns every schedule read publishes, defined once.
macro_rules! eta_override_select_sql {
    () => {
        concat!(
            "COALESCE(
                    CASE WHEN ",
            eta_override_in_force_sql!(),
            "
                        THEN bstd.eta_override_variant_id
                    END,
                    CASE
                        WHEN w.is_flexi THEN NULL
                        WHEN EXISTS (SELECT 1 FROM eta_variant v
                                     WHERE v.variant_id = bstd.default_variant_id
                                       AND NOT v.deleted)
                        THEN bstd.default_variant_id
                    END
                ) AS effective_variant_id,
                CASE WHEN ",
            eta_override_in_force_sql!(),
            "
                    THEN bstd.eta_override_effective_untill
                END AS override_effective_untill"
        )
    };
}

/// For the ops guards, which join no waybill: is this override still reaching a live run? The run
/// is the test, not the clock -- a window can close while the run it covers is still under way.
macro_rules! eta_override_has_live_run_sql {
    () => {
        concat!(
            r#"EXISTS (SELECT 1 FROM waybills_internal w
                          WHERE w.schedule_trip_id = bstd.schedule_trip_id
                            AND w.gtfs_id = bstd.gtfs_id
                            AND w.deleted = false
                            AND w.status IN ('online', 'upcoming')
                            AND "#,
            eta_override_window_overlap_sql!(
                "bstd.eta_override_effective_from",
                "bstd.eta_override_effective_untill"
            ),
            ")"
        )
    };
}

/// The join conditions every schedule read applies, so a write cannot land on a row resolution
/// never surfaces -- a dead or inactive trip, or a flexi waybill, which reads its trips elsewhere.
macro_rules! eta_override_trip_eligible_sql {
    () => {
        r#"w.is_flexi = false
              AND bstd.deleted = false
              AND bstd.trip_type <> 'dead-trip'
              AND (w.status <> 'online' OR LOWER(COALESCE(bstd.status, 'active')) <> 'inactive')"#
    };
}

pub(crate) use eta_override_in_force_sql;
pub(crate) use eta_override_select_sql;
pub(crate) use eta_override_window_overlap_sql;

const STATION_ETA_CACHE_DURATION: u64 = 1800; // 30 mins
const ETA_VARIANT_CACHE_DURATION: u64 = 1800; // catalogue changes as rarely as the times it labels
/// gtfs_id is request-path input, so both caches are bounded: an unknown feed must not be able
/// to leave a permanent entry behind. Far above the handful of real feeds.
const ETA_CACHE_MAX_FEEDS: usize = 256;

impl DBVehicleReaderInternal {
    pub(super) fn is_station_eta_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(STATION_ETA_CACHE_DURATION)
    }

    pub(super) fn get_station_eta_cache_key(&self, gtfs_id: &str) -> String {
        format!("eta_map_{}", gtfs_id)
    }

    /// Segment times for a feed, grouped by variant. Reference data, so the whole feed is
    /// loaded once and held: which variant a given trip uses is decided per request against
    /// this map, never by re-querying.
    pub async fn get_station_etas_impl(&self, gtfs_id: &str) -> AppResult<Arc<StationEtaMap>> {
        let cache_key = self.get_station_eta_cache_key(gtfs_id);

        // Check cache
        {
            let cache = self.station_eta_cache.read().await;
            if let Some((etas, timestamp)) = cache.get(&cache_key) {
                if !self.is_station_eta_cache_expired(*timestamp) {
                    debug!("station_eta_cache HIT for gtfs_id={}", gtfs_id);
                    return Ok(Arc::clone(etas));
                }
            }
        }

        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Arc::new(StationEtaMap::new())),
        };

        let query = r#"
            SELECT s.variant_id, s.source_station_code, s.destination_station_code, s.eta_in_seconds
            FROM station_eta s
            JOIN eta_variant v ON v.variant_id = s.variant_id AND NOT v.deleted
            WHERE s.gtfs_id = $1
        "#;

        let rows = sqlx::query(query)
            .bind(gtfs_id)
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        let mut eta_map = StationEtaMap::new();
        for row in rows {
            use sqlx::Row;
            // try_get, not get: a schema skew fails this read instead of panicking the worker.
            let variant: String = row
                .try_get("variant_id")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let src: String = row
                .try_get("source_station_code")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let dst: String = row
                .try_get("destination_station_code")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            let secs: i32 = row
                .try_get("eta_in_seconds")
                .map_err(|e| AppError::DbError(e.to_string()))?;
            eta_map.entry(variant).or_default().insert((src, dst), secs);
        }

        let eta_map = Arc::new(eta_map);
        {
            let mut cache = self.station_eta_cache.write().await;
            if cache.len() >= ETA_CACHE_MAX_FEEDS {
                cache.retain(|_, (_, stamp)| !self.is_station_eta_cache_expired(*stamp));
            }
            if cache.len() < ETA_CACHE_MAX_FEEDS {
                cache.insert(cache_key, (Arc::clone(&eta_map), SystemTime::now()));
            }
        }

        Ok(eta_map)
    }

    pub(super) async fn invalidate_station_eta_cache(&self, gtfs_id: &str) {
        let key = self.get_station_eta_cache_key(gtfs_id);
        self.station_eta_cache.write().await.remove(&key);
    }

    pub(super) fn is_eta_variant_cache_expired(&self, timestamp: SystemTime) -> bool {
        let elapsed = timestamp.elapsed().unwrap_or_default();
        elapsed >= Duration::from_secs(ETA_VARIANT_CACHE_DURATION)
    }

    pub(super) async fn get_eta_variants_impl(&self, gtfs_id: &str) -> AppResult<Vec<EtaVariant>> {
        {
            let cache = self.eta_variant_cache.read().await;
            if let Some((variants, ts)) = cache.get(gtfs_id) {
                if !self.is_eta_variant_cache_expired(*ts) {
                    return Ok(variants.clone());
                }
            }
        }

        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        let variants = sqlx::query_as::<_, EtaVariant>(
            r#"
            SELECT variant_id, gtfs_id, code, display_name, is_default
            FROM eta_variant
            WHERE gtfs_id = $1 AND deleted = false
            ORDER BY is_default DESC, code
            "#,
        )
        .bind(gtfs_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        {
            let mut cache = self.eta_variant_cache.write().await;
            if cache.len() >= ETA_CACHE_MAX_FEEDS {
                cache.retain(|_, (_, stamp)| !self.is_eta_variant_cache_expired(*stamp));
            }
            if cache.len() < ETA_CACHE_MAX_FEEDS {
                cache.insert(gtfs_id.to_string(), (variants.clone(), SystemTime::now()));
            }
        }

        Ok(variants)
    }

    pub(super) async fn invalidate_eta_variant_cache(&self, gtfs_id: &str) {
        self.eta_variant_cache.write().await.remove(gtfs_id);
    }

    /// Resolve the variant a write targets: the caller's if given, else the feed's default.
    pub(super) async fn resolve_write_variant(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
    ) -> AppResult<String> {
        // Blank means "not given" everywhere else the API takes a variantId, so a caller that
        // sends "" gets the feed default rather than a lookup failure on the empty string.
        match variant_id.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => {
                // Deliberately not get_eta_variants_impl: its 30-min cache is per-process, so
                // one replica would reject a variant another just created (and accept one it
                // just deleted). Ops writes are rare enough to always ask the database.
                let known: bool = sqlx::query_scalar(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM eta_variant
                        WHERE gtfs_id = $1 AND variant_id = $2 AND NOT deleted
                    )
                    "#,
                )
                .bind(gtfs_id)
                .bind(v)
                .fetch_one(self.pool()?)
                .await
                .map_err(|e| AppError::DbError(e.to_string()))?;
                if known {
                    Ok(v.to_string())
                } else {
                    Err(AppError::BadRequest(format!(
                        "Unknown or deleted eta variant '{}' for gtfs_id '{}'",
                        v, gtfs_id
                    )))
                }
            }
            None => sqlx::query_scalar::<_, String>(
                r#"
                SELECT variant_id FROM eta_variant
                WHERE gtfs_id = $1 AND is_default AND NOT deleted
                "#,
            )
            .bind(gtfs_id)
            .fetch_optional(self.pool()?)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "No default eta variant configured for '{}'",
                    gtfs_id
                ))
            }),
        }
    }

    pub(super) async fn upsert_station_etas_impl(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
        entries: &[StationEtaEntry],
    ) -> AppResult<u64> {
        if entries.is_empty() {
            return Ok(0);
        }

        let pool = self
            .pool
            .as_ref()
            .ok_or_else(|| AppError::DbError("Internal Database pool is not active".into()))?;

        let variant = self.resolve_write_variant(gtfs_id, variant_id).await?;

        // ON CONFLICT DO UPDATE cannot touch the same row twice in one statement, so a batch
        // carrying a pair more than once has to be collapsed first — last value wins.
        let mut seen: HashMap<(&str, &str), i32> = HashMap::with_capacity(entries.len());
        for e in entries {
            if e.eta_in_seconds <= 0 {
                return Err(AppError::BadRequest(format!(
                    "eta_in_seconds must be positive (got {} for {} -> {})",
                    e.eta_in_seconds, e.source_station_code, e.destination_station_code
                )));
            }
            seen.insert(
                (
                    e.source_station_code.as_str(),
                    e.destination_station_code.as_str(),
                ),
                e.eta_in_seconds,
            );
        }

        let mut srcs: Vec<&str> = Vec::with_capacity(seen.len());
        let mut dsts: Vec<&str> = Vec::with_capacity(seen.len());
        let mut secs: Vec<i32> = Vec::with_capacity(seen.len());
        for ((s, d), v) in &seen {
            srcs.push(s);
            dsts.push(d);
            secs.push(*v);
        }

        let result = sqlx::query(
            r#"
            INSERT INTO station_eta (gtfs_id, variant_id, source_station_code, destination_station_code, eta_in_seconds)
            SELECT $1, $2, s.src, s.dst, s.secs
            FROM UNNEST($3::text[], $4::text[], $5::int[]) AS s(src, dst, secs)
            ON CONFLICT (gtfs_id, variant_id, source_station_code, destination_station_code)
            DO UPDATE SET
                eta_in_seconds = EXCLUDED.eta_in_seconds,
                updated_at = CURRENT_TIMESTAMP
            "#,
        )
        .bind(gtfs_id)
        .bind(&variant)
        .bind(&srcs)
        .bind(&dsts)
        .bind(&secs)
        .execute(pool)
        .await
        .map_err(|e| {
            error!("Failed to upsert station_eta for gtfs_id={}: {}", gtfs_id, e);
            AppError::DbError(e.to_string())
        })?;

        self.invalidate_station_eta_cache(gtfs_id).await;

        info!(
            "Upserted {} station_eta pair(s) for gtfs_id={} variant={}",
            result.rows_affected(),
            gtfs_id,
            variant
        );

        Ok(result.rows_affected())
    }

    pub(super) async fn upsert_eta_variant_impl(
        &self,
        variant: &EtaVariant,
    ) -> AppResult<EtaVariant> {
        let pool = self.pool()?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        // eta_variant_one_default_idx stops a feed having two defaults but not zero, and
        // ON CONFLICT writes is_default unconditionally, so demoting the current default
        // would strand the feed with none and drop every unpinned trip to haversine.
        let had_default: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1 FROM eta_variant WHERE gtfs_id = $1 AND is_default AND NOT deleted
            )
            "#,
        )
        .bind(&variant.gtfs_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        // A feed with variants but no default resolves nothing: station_eta writes fail and
        // reads fall through to geometric. The first variant in therefore becomes the default.
        let is_default = variant.is_default || !had_default;

        // Displacing a working default with an empty variant drops the feed to haversine, which
        // delete_eta_variant refuses for the same reason. A feed's first default has no times yet.
        if is_default && had_default {
            let has_times: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM station_eta s
                    JOIN eta_variant v ON v.variant_id = s.variant_id
                    WHERE v.gtfs_id = $1 AND v.code = $2 AND NOT v.deleted
                )
                "#,
            )
            .bind(&variant.gtfs_id)
            .bind(&variant.code)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

            if !has_times {
                return Err(AppError::BadRequest(format!(
                    "Variant '{}' has no segment times, so making it the default would drop \
                     {} to straight-line ETAs; add its times first, then set it as default",
                    variant.code, variant.gtfs_id
                )));
            }
        }

        // eta_variant_one_default_idx permits a single default per feed, so an incoming
        // default has to displace the current one inside the same transaction.
        if is_default {
            sqlx::query(
                r#"
                UPDATE eta_variant
                SET is_default = false, updated_at = now()
                WHERE gtfs_id = $1 AND is_default AND NOT deleted AND code <> $2
                "#,
            )
            .bind(&variant.gtfs_id)
            .bind(&variant.code)
            .execute(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;
        }

        let saved = sqlx::query_as::<_, EtaVariant>(
            r#"
            INSERT INTO eta_variant
                (variant_id, gtfs_id, code, display_name, is_default)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (gtfs_id, code) WHERE (NOT deleted)
            DO UPDATE SET
                display_name = EXCLUDED.display_name,
                is_default = EXCLUDED.is_default,
                updated_at = now()
            RETURNING variant_id, gtfs_id, code, display_name, is_default
            "#,
        )
        .bind(&variant.variant_id)
        .bind(&variant.gtfs_id)
        .bind(&variant.code)
        .bind(&variant.display_name)
        .bind(is_default)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if had_default {
            let still_has_default: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM eta_variant WHERE gtfs_id = $1 AND is_default AND NOT deleted
                )
                "#,
            )
            .bind(&variant.gtfs_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

            if !still_has_default {
                return Err(AppError::BadRequest(format!(
                    "'{}' would be left with no default eta variant; promote another variant to default instead of clearing this one",
                    variant.gtfs_id
                )));
            }
        }

        tx.commit()
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

        self.invalidate_eta_variant_cache(&variant.gtfs_id).await;
        Ok(saved)
    }

    pub(super) async fn delete_eta_variant_impl(
        &self,
        gtfs_id: &str,
        variant_id: &str,
    ) -> AppResult<u64> {
        let pool = self.pool()?;

        // Retiring the default would leave every trip that names no variant with nowhere to
        // fall back to, silently dropping the whole feed to haversine.
        let result = sqlx::query(concat!(
            r#"
            UPDATE eta_variant
            SET deleted = true, updated_at = now()
            WHERE gtfs_id = $1 AND variant_id = $2 AND NOT deleted AND NOT is_default
              AND NOT EXISTS (
                  SELECT 1 FROM bus_schedule_trip_detail_internal bstd
                  WHERE bstd.gtfs_id = eta_variant.gtfs_id
                    AND bstd.deleted = false
                    AND (bstd.default_variant_id = eta_variant.variant_id
                         OR (bstd.eta_override_variant_id = eta_variant.variant_id
                             AND "#,
            eta_override_has_live_run_sql!(),
            r#"))
              )
            "#,
        ))
        .bind(gtfs_id)
        .bind(variant_id)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // The guard above is folded into the UPDATE so a concurrent override cannot slip in
            // between check and delete; the count is re-read only to say which rule was hit.
            let in_use: i64 = sqlx::query_scalar(concat!(
                r#"
                SELECT count(*) FROM bus_schedule_trip_detail_internal bstd
                WHERE bstd.gtfs_id = $1
                  AND bstd.deleted = false
                  AND (bstd.default_variant_id = $2
                       OR (bstd.eta_override_variant_id = $2
                           AND "#,
                eta_override_has_live_run_sql!(),
                r#"))
                "#,
            ))
            .bind(gtfs_id)
            .bind(variant_id)
            .fetch_one(pool)
            .await
            .unwrap_or(0);

            if in_use > 0 {
                return Err(AppError::BadRequest(format!(
                    "Variant '{}' is still in force on {} trip(s) as an override or a schedule default; clear those first",
                    variant_id, in_use
                )));
            }
            return Err(AppError::BadRequest(format!(
                "Variant '{}' is unknown, already deleted, or is the default for '{}'",
                variant_id, gtfs_id
            )));
        }

        self.invalidate_eta_variant_cache(gtfs_id).await;
        // The segment-time map is keyed by variant and built with a NOT deleted join, so the
        // cached copy still holds the retired variant's rows until it is dropped.
        self.invalidate_station_eta_cache(gtfs_id).await;
        Ok(result.rows_affected())
    }

    /// Validates the variant the same way an override does, so a schedule cannot be pinned to
    /// one that resolution would then ignore.
    pub(super) async fn set_schedule_default_variant_impl(
        &self,
        gtfs_id: &str,
        schedule_trip_id: &str,
        trip_number: Option<i32>,
        variant_id: Option<&str>,
    ) -> AppResult<u64> {
        let pool = self.pool()?;
        // Absent and blank both mean "clear the pin" here, rather than blank resolving to the
        // feed default the way it does when a write is naming a variant to store against.
        let variant = match variant_id.map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => Some(self.resolve_write_variant(gtfs_id, Some(v)).await?),
            None => None,
        };

        // trip_number None sets every trip of the schedule, which is how a whole service is
        // pinned to a variant in one call.
        let result = sqlx::query(
            r#"
            UPDATE bus_schedule_trip_detail_internal
            SET default_variant_id = $3
            WHERE gtfs_id = $1
              AND schedule_trip_id = $2
              AND ($4::int IS NULL OR trip_number = $4)
              AND deleted = false
            "#,
        )
        .bind(gtfs_id)
        .bind(schedule_trip_id)
        .bind(variant.as_deref())
        .bind(trip_number)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "No schedule trip '{}' for gtfs_id '{}'",
                schedule_trip_id, gtfs_id
            )));
        }
        Ok(result.rows_affected())
    }

    pub(super) async fn set_trip_eta_override_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
        variant_id: &str,
        effective_from: chrono::DateTime<chrono::Utc>,
        effective_untill: chrono::DateTime<chrono::Utc>,
    ) -> AppResult<()> {
        let pool = self.pool()?;
        if variant_id.trim().is_empty() {
            return Err(AppError::BadRequest(
                "variantId must name a variant; an override cannot mean \"the default\""
                    .to_string(),
            ));
        }
        let variant = self
            .resolve_write_variant(gtfs_id, Some(variant_id))
            .await?;

        // Requiring the overlap here: a window the addressed run falls outside would store fine
        // and never apply.
        let result = sqlx::query(concat!(
            r#"
            UPDATE bus_schedule_trip_detail_internal bstd
            SET eta_override_variant_id = $4,
                eta_override_effective_from = $5,
                eta_override_effective_untill = $6
            FROM waybills_internal w
            WHERE w.gtfs_id = $1
              AND w.waybill_no::text = $2
              AND w.deleted = false
              AND w.status IN ('online', 'upcoming')
              AND bstd.schedule_trip_id = w.schedule_trip_id
              AND bstd.gtfs_id = $1
              AND bstd.trip_number = $3
              AND "#,
            eta_override_trip_eligible_sql!(),
            r#"
              AND "#,
            eta_override_window_overlap_sql!("$5", "$6"),
        ))
        .bind(gtfs_id)
        .bind(waybill_no)
        .bind(trip_number)
        .bind(&variant)
        .bind(effective_from)
        .bind(effective_untill)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // "Trip not found" and "window missed the run" need different fixes from the caller.
            // Same eligibility as the write, so an ineligible trip is not blamed on the window.
            let run: Option<(String, String, String)> = sqlx::query_as(concat!(
                r#"
                SELECT COALESCE(w.duty_date, '?'),
                       COALESCE(bstd.start_time, '?'),
                       COALESCE(bstd.end_time, '?')
                FROM bus_schedule_trip_detail_internal bstd
                JOIN waybills_internal w
                    ON w.schedule_trip_id = bstd.schedule_trip_id
                    AND w.gtfs_id = bstd.gtfs_id
                WHERE w.gtfs_id = $1
                  AND w.waybill_no::text = $2
                  AND w.deleted = false
                  AND w.status IN ('online', 'upcoming')
                  AND bstd.trip_number = $3
                  AND "#,
                eta_override_trip_eligible_sql!(),
                r#"
                ORDER BY (w.status = 'online') DESC
                LIMIT 1
                "#,
            ))
            .bind(gtfs_id)
            .bind(waybill_no)
            .bind(trip_number)
            .fetch_optional(pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

            return match run {
                Some((duty_date, start_time, end_time)) => Err(AppError::BadRequest(format!(
                    "Window {} to {} covers no run of trip {} on waybill '{}'; that trip runs \
                     {} {}-{} IST",
                    effective_from,
                    effective_untill,
                    trip_number,
                    waybill_no,
                    duty_date,
                    start_time,
                    end_time
                ))),
                None => Err(AppError::NotFound(format!(
                    "No trip {} on a live waybill '{}' for gtfs_id '{}'",
                    trip_number, waybill_no, gtfs_id
                ))),
            };
        }

        info!(
            "Set eta override gtfs_id={} waybill_no={} trip={} variant={} from={} untill={} rows={}",
            gtfs_id,
            waybill_no,
            trip_number,
            variant,
            effective_from,
            effective_untill,
            result.rows_affected()
        );
        Ok(())
    }

    pub(super) async fn clear_trip_eta_override_impl(
        &self,
        gtfs_id: &str,
        waybill_no: &str,
        trip_number: i32,
    ) -> AppResult<u64> {
        let pool = self.pool()?;

        let result = sqlx::query(
            r#"
            UPDATE bus_schedule_trip_detail_internal bstd
            SET eta_override_variant_id = NULL,
                eta_override_effective_from = NULL,
                eta_override_effective_untill = NULL
            FROM waybills_internal w
            WHERE w.gtfs_id = $1
              AND w.waybill_no::text = $2
              AND w.deleted = false
              AND bstd.schedule_trip_id = w.schedule_trip_id
              AND bstd.gtfs_id = $1
              AND bstd.trip_number = $3
              AND bstd.deleted = false
              AND bstd.eta_override_variant_id IS NOT NULL
            "#,
        )
        .bind(gtfs_id)
        .bind(waybill_no)
        .bind(trip_number)
        .execute(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))?;

        if result.rows_affected() == 0 {
            // Zero rows is "already clear" or "no such trip"; a mistyped waybill must not read as
            // success. Unfiltered like the UPDATE, so a trip stays clearable once offline.
            let exists: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM bus_schedule_trip_detail_internal bstd
                    JOIN waybills_internal w
                        ON w.schedule_trip_id = bstd.schedule_trip_id
                        AND w.gtfs_id = bstd.gtfs_id
                    WHERE w.gtfs_id = $1
                      AND w.waybill_no::text = $2
                      AND w.deleted = false
                      AND bstd.trip_number = $3
                      AND bstd.deleted = false
                )
                "#,
            )
            .bind(gtfs_id)
            .bind(waybill_no)
            .bind(trip_number)
            .fetch_one(pool)
            .await
            .map_err(|e| AppError::DbError(e.to_string()))?;

            if !exists {
                return Err(AppError::NotFound(format!(
                    "No trip {} on waybill '{}' for gtfs_id '{}'",
                    trip_number, waybill_no, gtfs_id
                )));
            }
        }

        Ok(result.rows_affected())
    }

    pub(super) async fn list_active_trip_eta_overrides_impl(
        &self,
        gtfs_id: &str,
    ) -> AppResult<Vec<ActiveTripEtaOverride>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        // Ops-facing, not the hot path: served straight from the schedule rows carrying an
        // override, which idx_bstd_eta_override_active keeps to a handful.
        //
        // One row per affected run: a window may cover several duty dates of the same trip, and
        // collapsing them would pick a waybill arbitrarily.
        sqlx::query_as::<_, ActiveTripEtaOverride>(concat!(
            r#"
            SELECT
                w.waybill_no::text AS waybill_no,
                bstd.trip_number,
                bstd.route_number_id::text AS route_id,
                bstd.eta_override_variant_id AS variant_id,
                bstd.eta_override_effective_from AS effective_from,
                bstd.eta_override_effective_untill AS effective_untill
            FROM bus_schedule_trip_detail_internal bstd
            JOIN waybills_internal w
                ON w.schedule_trip_id = bstd.schedule_trip_id
                AND w.gtfs_id = bstd.gtfs_id
                AND w.deleted = false
                AND w.waybill_no IS NOT NULL
                -- Only live waybills can still be affected.
                AND w.status IN ('online', 'upcoming')
            WHERE bstd.gtfs_id = $1
              -- Not also filtered on the window having closed: it can close mid-run, and rider-app
              -- keys its cache off this listing, so an omission there goes unnoticed.
              AND bstd.eta_override_variant_id IS NOT NULL
              -- Same eligibility resolution applies.
              AND "#,
            eta_override_trip_eligible_sql!(),
            r#"
              AND "#,
            eta_override_in_force_sql!(),
            r#"
            ORDER BY w.waybill_no, bstd.trip_number
            "#,
        ))
        .bind(gtfs_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))
    }

    /// The stored segment times themselves, for the screen that edits them. Deliberately
    /// uncached: this backs an editing screen, where ops must see the row they just saved
    /// rather than a copy up to the cache TTL old. Retired variants never appear, matching
    /// what resolution sees.
    pub async fn list_station_etas_impl(
        &self,
        gtfs_id: &str,
        variant_id: Option<&str>,
    ) -> AppResult<Vec<StationEtaRow>> {
        let pool = match &self.pool {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };

        sqlx::query_as::<_, StationEtaRow>(
            r#"
            SELECT s.variant_id, s.source_station_code, s.destination_station_code,
                   s.eta_in_seconds
            FROM station_eta s
            JOIN eta_variant v ON v.variant_id = s.variant_id AND NOT v.deleted
            WHERE s.gtfs_id = $1
              AND ($2::text IS NULL OR s.variant_id = $2)
            ORDER BY s.variant_id, s.source_station_code, s.destination_station_code
            "#,
        )
        .bind(gtfs_id)
        .bind(variant_id)
        .fetch_all(pool)
        .await
        .map_err(|e| AppError::DbError(e.to_string()))
    }
}

#[cfg(test)]
mod override_sql_tests {
    /// `AT TIME ZONE '+05:30'` is POSIX-signed and shifts the trip clock 11 hours the wrong way.
    #[test]
    fn window_math_never_uses_an_offset_string() {
        for sql in [
            super::eta_override_select_sql!(),
            super::eta_override_window_overlap_sql!("$1", "$2"),
        ] {
            assert!(sql.contains("AT TIME ZONE 'Asia/Kolkata'"));
            assert!(
                !sql.contains("+05:30"),
                "offset strings are POSIX-signed: {}",
                sql
            );
        }
    }

    /// Without the roll, a past-midnight trip's end precedes its start and never matches.
    #[test]
    fn window_math_rolls_trips_that_cross_midnight() {
        let sql = super::eta_override_window_overlap_sql!("$1", "$2");
        assert!(sql.contains("bstd.end_time::time < bstd.start_time::time"));
        assert!(sql.contains("THEN interval '1 day'"));
    }

    /// A malformed duty_date must not abort a schedule read that is mostly override-free.
    #[test]
    fn window_math_guards_its_casts() {
        let sql = super::eta_override_window_overlap_sql!("$1", "$2");
        let guard = sql.find("!~").expect("format guard");
        let cast = sql.find("::timestamp").expect("timestamp cast");
        assert!(guard < cast, "regex guard must precede the cast");
        assert_eq!(
            sql.matches('(').count(),
            sql.matches(')').count(),
            "unbalanced parens would break every query splicing this in"
        );
    }

    /// A variant still reaching an in-progress run cannot be retired out from under it.
    #[test]
    fn live_run_guard_joins_a_waybill_and_balances() {
        let sql = eta_override_has_live_run_sql!();
        assert!(sql.contains("FROM waybills_internal w"));
        assert!(sql.contains("w.status IN ('online', 'upcoming')"));
        assert!(sql.contains("AT TIME ZONE 'Asia/Kolkata'"));
        assert_eq!(sql.matches('(').count(), sql.matches(')').count());
    }

    /// Read path compares the stored window, write path the proposed one.
    #[test]
    fn window_math_takes_its_bounds_from_the_caller() {
        let stored = super::eta_override_window_overlap_sql!(
            "bstd.eta_override_effective_from",
            "bstd.eta_override_effective_untill"
        );
        let proposed = super::eta_override_window_overlap_sql!("$5", "$6");
        assert!(stored.contains("<= bstd.eta_override_effective_untill"));
        assert!(proposed.contains("<= $6") && proposed.contains(">= $5"));
    }
}
