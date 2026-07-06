use crate::environment::AppConfig;
use crate::models::MinimalEmployee;
use crate::tools::error::{AppError, AppResult};
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::debug;

/// Minimal employee row used by mobile-number login. Depot info is resolved
/// separately via the depot cache using `entity_id`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EmployeeLookupRow {
    pub token_no: Option<String>,
    pub first_name: String,
    pub last_name: Option<String>,
    pub mobile_no: Option<String>,
    pub entity_id: i64,
    pub designation_name: Option<String>,
}

#[async_trait]
pub trait EmployeeReader: Send + Sync {
    async fn get_employee_by_phone(&self, phone: &str) -> AppResult<Option<MinimalEmployee>>;

    /// Look up an employee in primary `employees` joined with `designations`.
    async fn lookup_by_mobile_primary(
        &self,
        mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>>;

    /// Look up an employee in `employees_internal` for the given gtfs_id.
    async fn lookup_by_mobile_internal(
        &self,
        gtfs_id: &str,
        mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>>;
}

pub struct MockEmployeeReader;

impl Default for MockEmployeeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MockEmployeeReader {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EmployeeReader for MockEmployeeReader {
    async fn get_employee_by_phone(&self, _phone: &str) -> AppResult<Option<MinimalEmployee>> {
        Err(AppError::NotFound(
            "Employee lookup disabled in mock mode".into(),
        ))
    }

    async fn lookup_by_mobile_primary(
        &self,
        _mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>> {
        // Mock mode has no data; report a miss rather than an error.
        // Matches the MockDBVehicleReader::get_depot_info convention.
        Ok(None)
    }

    async fn lookup_by_mobile_internal(
        &self,
        _gtfs_id: &str,
        _mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>> {
        Ok(None)
    }
}

pub struct DBEmployeeReader {
    pool: PgPool,
    internal_pool: Option<PgPool>,
    cache: Arc<RwLock<HashMap<String, (MinimalEmployee, SystemTime)>>>,
    lookup_cache: Arc<RwLock<HashMap<String, (EmployeeLookupRow, SystemTime)>>>,
    cache_duration: Duration,
}

impl DBEmployeeReader {
    pub fn new(pool: PgPool, internal_pool: Option<PgPool>, config: &AppConfig) -> Self {
        Self {
            pool,
            internal_pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            lookup_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
        }
    }

    async fn get_cached_employee(&self, phone: &str) -> Option<MinimalEmployee> {
        let cache = self.cache.read().await;
        if let Some((employee, timestamp)) = cache.get(phone) {
            if timestamp.elapsed().unwrap_or_default() < self.cache_duration {
                debug!("Cache HIT for employee with phone {}", phone);
                return Some(employee.clone());
            }
        }
        debug!("Cache MISS for employee with phone {}", phone);
        None
    }

    async fn cache_employee_data(&self, employee: &MinimalEmployee, phone: &str) {
        let mut cache = self.cache.write().await;
        cache.insert(phone.to_string(), (employee.clone(), SystemTime::now()));
    }

    async fn get_cached_lookup(&self, key: &str) -> Option<EmployeeLookupRow> {
        let cache = self.lookup_cache.read().await;
        if let Some((row, timestamp)) = cache.get(key) {
            if timestamp.elapsed().unwrap_or_default() < self.cache_duration {
                debug!("Cache HIT for employee lookup {}", key);
                return Some(row.clone());
            }
        }
        debug!("Cache MISS for employee lookup {}", key);
        None
    }

    async fn cache_lookup(&self, key: &str, row: &EmployeeLookupRow) {
        let mut cache = self.lookup_cache.write().await;
        cache.insert(key.to_string(), (row.clone(), SystemTime::now()));
    }
}

#[async_trait]
impl EmployeeReader for DBEmployeeReader {
    async fn get_employee_by_phone(&self, phone: &str) -> AppResult<Option<MinimalEmployee>> {
        if let Some(cached_employee) = self.get_cached_employee(phone).await {
            return Ok(Some(cached_employee));
        }

        // Primary lookup in employees table
        let primary_query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                ent.entity_remark AS depot_name
            FROM employees e
            LEFT JOIN entities ent ON ent.entity_id = e.entity_id
            WHERE e.mobile_no = $1
              AND e.deleted = false
            LIMIT 1
        "#;

        match sqlx::query_as::<_, MinimalEmployee>(primary_query)
            .bind(phone)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(emp)) => {
                debug!("Employee found in employees table for phone {}", phone);
                self.cache_employee_data(&emp, phone).await;
                return Ok(Some(emp));
            }
            Ok(None) => {}
            Err(e) => return Err(AppError::Internal(format!("DB query error: {}", e))),
        }

        // Fallback lookup in employees_internal table
        let internal_pool = match &self.internal_pool {
            Some(p) => p,
            None => return Ok(None),
        };

        let internal_query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                ent.entity_remark AS depot_name
            FROM employees_internal e
            LEFT JOIN entities_internal ent ON ent.entity_id = e.entity_id
            WHERE e.mobile_no = $1
              AND e.deleted = false
            LIMIT 1
        "#;

        match sqlx::query_as::<_, MinimalEmployee>(internal_query)
            .bind(phone)
            .fetch_optional(internal_pool)
            .await
        {
            Ok(Some(emp)) => {
                debug!(
                    "Employee found in employees_internal table for phone {}",
                    phone
                );
                self.cache_employee_data(&emp, phone).await;
                Ok(Some(emp))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(AppError::Internal(format!(
                "DB query error (internal): {}",
                e
            ))),
        }
    }

    async fn lookup_by_mobile_primary(
        &self,
        mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>> {
        let cache_key = format!("primary:{}", mobile_no);
        if let Some(cached) = self.get_cached_lookup(&cache_key).await {
            return Ok(Some(cached));
        }

        // Not single-flighted: concurrent first hits for the same phone will all
        // query before any writes back to the cache. Acceptable — the dupes only
        // cost an extra DB round trip on a cold key.
        //
        // ORDER BY emp_id DESC makes the lookup deterministic when stale duplicate
        // rows share a mobile_no (the schema does not enforce uniqueness): prefer
        // the most recently inserted row, which is the one operations meant to
        // activate.
        let query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                e.entity_id,
                LOWER(d.designation_name) AS designation_name
            FROM employees e
            LEFT JOIN designations d ON d.designation_id = e.designation_id
                                    AND d.deleted = false
            WHERE e.mobile_no = $1
              AND e.deleted = false
            ORDER BY e.emp_id DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, EmployeeLookupRow>(query)
            .bind(mobile_no)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(row)) => {
                debug!("Employee lookup primary HIT for mobile_no {}", mobile_no);
                self.cache_lookup(&cache_key, &row).await;
                Ok(Some(row))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(AppError::Internal(format!(
                "DB query error (primary mobile lookup): {}",
                e
            ))),
        }
    }

    async fn lookup_by_mobile_internal(
        &self,
        gtfs_id: &str,
        mobile_no: &str,
    ) -> AppResult<Option<EmployeeLookupRow>> {
        let cache_key = format!("internal:{}:{}", gtfs_id, mobile_no);
        if let Some(cached) = self.get_cached_lookup(&cache_key).await {
            return Ok(Some(cached));
        }

        let internal_pool = match &self.internal_pool {
            Some(p) => p,
            None => return Ok(None),
        };

        // Not single-flighted, deterministic on duplicates (see lookup_by_mobile_primary).
        let query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                e.entity_id,
                LOWER(d.designation_name) AS designation_name
            FROM employees_internal e
            LEFT JOIN designations_internal d ON d.designation_id = e.designation_id
                                             AND d.gtfs_id = e.gtfs_id
                                             AND d.deleted = false
            WHERE e.mobile_no = $1
              AND e.gtfs_id = $2
              AND e.deleted = false
            ORDER BY e.emp_id DESC
            LIMIT 1
        "#;

        match sqlx::query_as::<_, EmployeeLookupRow>(query)
            .bind(mobile_no)
            .bind(gtfs_id)
            .fetch_optional(internal_pool)
            .await
        {
            Ok(Some(row)) => {
                debug!(
                    "Employee lookup internal HIT for gtfs_id={} mobile_no={}",
                    gtfs_id, mobile_no
                );
                self.cache_lookup(&cache_key, &row).await;
                Ok(Some(row))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(AppError::Internal(format!(
                "DB query error (internal mobile lookup): {}",
                e
            ))),
        }
    }
}
